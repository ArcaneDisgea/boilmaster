use std::sync::Arc;

use anyhow::Context;
use axum::{extract::Query, response::IntoResponse, routing::get, Extension, Json, Router};
use axum_macros::debug_handler;
use ironworks_schema::saint_coinach;
use serde::{Deserialize, Serialize};

use crate::{
	data::Data,
	search::{query, Search},
};

use super::error::Result;

pub fn router(search_service: Arc<Search>) -> Router {
	Router::new()
		.route("/", get(search))
		.layer(Extension(search_service))
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
	query: query::pre::Node,
}

// TODO: flesh this out - at the moment it's just a 1:1 of searchresult, pending ideas on how to field filter for search results across multiple indices
#[derive(Debug, Serialize)]
struct SearchResult {
	score: f32,
	sheet: String,
	row_id: u32,
	subrow_id: u16,
}

#[debug_handler]
async fn search(
	Extension(search): Extension<Arc<Search>>,
	Extension(data): Extension<Arc<Data>>,
	Query(search_query): Query<SearchQuery>,
) -> Result<impl IntoResponse> {
	// TODO: this should expose a more useful error to the end user.
	let search_version = search.version(None).context("search index not ready")?;
	let excel = data.version(None).excel();

	// TODO: this should, like with ./sheets.rs, be pulled from some shared resource with proper versioning.
	let provider = saint_coinach::Provider::new()?;
	let version = provider.version("HEAD")?;

	let (results, warnings) = search_version
		.search(&search_query.query, &excel, &version)?
		.decompose();

	let http_results = results
		.into_iter()
		.map(|result| SearchResult {
			score: result.score,
			sheet: result.sheet,
			row_id: result.row_id,
			subrow_id: result.subrow_id,
		})
		.collect::<Vec<_>>();

	Ok(Json((http_results, warnings)))
}
