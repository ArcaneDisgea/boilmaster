use std::{
	borrow::Cow,
	cmp::Ordering,
	collections::{HashMap, HashSet},
	sync::Arc,
};

use anyhow::Context;
use either::Either;
use ironworks::excel;
use ironworks_schema::Schema;
use itertools::Itertools;
use serde::Deserialize;
use tokio::select;
use tokio_util::sync::CancellationToken;

use crate::{data::Data, utility::warnings::Warnings, version::VersionKey};

use super::{
	error::{Error, Result},
	internal_query::{post, pre, Normalizer},
	tantivy,
};

#[derive(Debug, Deserialize)]
pub struct Config {
	pagination: PaginationConfig,
	tantivy: tantivy::Config,
}

#[derive(Debug, Deserialize)]
struct PaginationConfig {
	limit_default: u32,
	limit_max: u32,
}

#[derive(Debug)]
pub struct SearchResult {
	pub score: f32,
	// TODO: `String` here necessitates a copy of the sheet name for every result, which seems wasteful.
	pub sheet: String,
	pub row_id: u32,
	pub subrow_id: u16,
}

pub struct Search {
	pagination_config: PaginationConfig,

	provider: Arc<tantivy::Provider>,

	data: Arc<Data>,
}

impl Search {
	pub fn new(config: Config, data: Arc<Data>) -> Result<Self> {
		Ok(Self {
			pagination_config: config.pagination,
			provider: Arc::new(tantivy::Provider::new(config.tantivy)?),
			data,
		})
	}

	pub async fn start(&self, cancel: CancellationToken) -> Result<()> {
		let mut receiver = self.data.subscribe();
		self.ingest(cancel.child_token(), receiver.borrow().clone())
			.await?;

		loop {
			select! {
				Ok(_) = receiver.changed() => {
					self.ingest(cancel.child_token(), receiver.borrow().clone()).await?
				}
				_ = cancel.cancelled() => break,
			}
		}

		Ok(())
	}

	async fn ingest(&self, cancel: CancellationToken, versions: Vec<VersionKey>) -> Result<()> {
		// Get a list of all sheets in the provided versions.
		// TODO: This has more `.collect`s than i'd like, but given it's a fairly cold path, probably isn't a problem.
		let sheets = versions
			.into_iter()
			.map(|version| -> Result<_> {
				let data_version = self.data.version(version).with_context(|| {
					format!("version {version} announced for ingestion but not provided")
				})?;
				let excel = data_version.excel();
				let list = excel.list()?;

				list.iter()
					.map(|sheet_name| Ok((version, excel.sheet(sheet_name.to_string())?)))
					.collect::<Result<Vec<_>>>()
			})
			.flatten_ok()
			.collect::<Result<Vec<_>>>()?;

		// Fire off the ingestion in the provider.
		Arc::clone(&self.provider).ingest(cancel, sheets).await?;

		Ok(())
	}

	// TODO: This code path is effectively ported from the pre-multi-sheet search index implementation, and as such, eagerly splits queries by sheet, preventing the provider from using it's knowledge of grouping to reduce the number of queries executed. Given that no changes to index schema or ingestion behavior is required to improve this, I'm leaving this as a problem to solve if/when query speed can do with some measurable improvement.
	pub fn search(
		&self,
		version: VersionKey,
		query: &pre::Node,
		language: excel::Language,
		sheet_filter: Option<HashSet<String>>,
		limit: Option<u32>,
		schema: &dyn Schema,
	) -> Result<Warnings<Vec<SearchResult>>> {
		// Get references to the game data we'll need.
		let excel = self
			.data
			.version(version)
			.with_context(|| format!("data for version {version} not ready"))?
			.excel();
		let list = excel.list()?;

		// Work out the actual result limit we'll use for this query.
		let limit = limit
			.unwrap_or(self.pagination_config.limit_default)
			.min(self.pagination_config.limit_max);
		// NOTE: This +1 is intentional - we intentionally request one more
		// than we'll actually return to make it trivial to distinguish when more
		// results exist, even when one index is suppling all data.
		let result_limit = limit + 1;

		// Build the helpers for this search call.
		let normalizer = Normalizer::new(&excel, schema);
		let executor = Executor {
			provider: &self.provider,
		};

		// Get an iterator over the provided sheet filter, falling back to the full list of sheets.
		let sheet_names = sheet_filter
			.map(|filter| Either::Left(filter.into_iter().map(Cow::from)))
			.unwrap_or_else(|| Either::Right(list.iter()));

		let index_results = sheet_names
			.map(|name| -> Result<_> {
				let normalized_query = normalizer.normalize(query, &name, language)?;
				let results =
					executor.search(version, &name, &normalized_query, Some(result_limit))?;
				let tagged_results = results.map(move |result| SearchResult {
					score: result.score,
					sheet: name.to_string(),
					row_id: result.row_id,
					subrow_id: result.subrow_id,
				});
				Ok(tagged_results)
			})
			.try_fold(Warnings::new(vec![]), |warnings, result| match result {
				// Successful search results can be pushed to the inner vector in the warnings.
				Ok(results) => Ok(warnings.map(|mut vec| {
					vec.push(results);
					vec
				})),
				// Failures should short circuit completely.
				Err(error @ Error::Failure(_)) => Err(error),
				// Query mismatches will be raised for most sheets, and aren't particularly meaningful for end-users. Skip.
				// TODO: ... right? i mean, it kind of sucks to not be able to say "oi this field doesn't exist" but... idk.
				Err(Error::QuerySchemaMismatch(_)) => Ok(warnings),
				// Other errors can be raised as warnings without halting the process.
				// TODO: find some way to tag this with the sheet name because at the moment the warnings are entirely unactionable.
				Err(error) => Ok(warnings.with_warning(error.to_string())),
			})?;

		// TODO: a zero-length array here implies all indices were query mismatches, or no index was queried at all. disambiguate and error out.
		// TODO: following the introduction of warnings; that's not quite right - it might all have ended up as warnings, too. While that's possibly _fine_ for i.e. a multi-sheet query, for a _single_ sheet query, it might be more-sane to raise as a top-level error. Think about it a bit, because... yeah. That's not exactly _consistent_ but maybe it's expected?

		// Run processing on the results.
		let results = index_results.map(|vec| {
			// Merge the results from each index into a single vector, sorting by score across all results.
			let mut results = vec.into_iter().flatten().collect::<Vec<_>>();
			results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));

			// If there's more results than required to fill the limit, then pagination should be permitted.
			let limit_usize = usize::try_from(limit).unwrap();
			let more_results = results.len() > limit_usize;

			// Cull any results sitting outside the limit.
			results.truncate(limit_usize);
			results.shrink_to_fit();

			// Only calculate cursor offsets if there's a need to specify a cursor.
			if more_results {
				let offsets =
					results
						.iter()
						.fold(HashMap::<String, u32>::new(), |mut map, result| {
							*map.entry(result.sheet.clone()).or_default() += 1;
							map
						});

				tracing::info!("more results! {offsets:#?}")
			}

			results
		});

		Ok(results)
	}
}

// TODO: can probably store the number of search executions on this to feed into rate limiting
pub struct Executor<'a> {
	provider: &'a tantivy::Provider,
}

impl Executor<'_> {
	// TODO: The Option on limit is to represent the "no limit" case required for inner queries in relationships, where outer filtering may lead to any theoretical bounded inner query to be insufficient. For obvious reasons this is... _not_ a particulary efficient approach, though I'm not sure what better approaches exist. If nothing else, would be good to cache common queries in memory to avoid constant repetition of unbounded limits.
	pub fn search(
		&self,
		version: VersionKey,
		sheet_name: &str,
		query: &post::Node,
		limit: Option<u32>,
	) -> Result<impl Iterator<Item = tantivy::IndexResult>> {
		self.provider
			.search(version, sheet_name, query, limit, self)
	}
}
