use std::collections::HashMap;

use nom::{
	branch::alt,
	bytes::complete::{tag, take_while1},
	combinator::{map, opt},
	multi::separated_list1,
	sequence::{delimited, preceded, tuple},
	IResult,
};
use serde::{de, Deserialize, Deserializer};

type StructFilter = HashMap<String, Option<ColumnFilter>>;
type ArrayFilter = Option<Box<ColumnFilter>>;

#[derive(Debug, PartialEq)]
pub enum ColumnFilter {
	Struct(StructFilter),

	// due to multiple slices, probably easiest to halt merges at arrays and only start merging again on index access
	Array(ArrayFilter),
	// do i want seperate syntax for references?
	// Reference
}

impl ColumnFilter {
	fn merge(self, source: Self) -> Self {
		match (self, source) {
			(Self::Struct(target_struct), Self::Struct(source_struct)) => {
				Self::Struct(merge_struct(target_struct, source_struct))
			}

			(Self::Array(target_array), Self::Array(source_array)) => {
				Self::Array(merge_array(target_array, source_array))
			}

			(fallback_1, fallback_2) => todo!("unhandled merge {fallback_1:?} <-> {fallback_2:?}"),
		}
	}
}

fn merge_struct(mut target: StructFilter, source: StructFilter) -> StructFilter {
	for (key, source_value) in source {
		let merged = match target.remove(&key) {
			// The target didn't contain this key yet, use the incoming value
			None => source_value,
			// We already had this key, perform a merge
			Some(target_value) => match (target_value, source_value) {
				// If both sides already had filters for this key, merge recursively
				(Some(target_filter), Some(source_filter)) => {
					Some(target_filter.merge(source_filter))
				}
				// If either side had None, which acts as an "All" value, propagate the None.
				_ => None,
			},
		};

		target.insert(key, merged);
	}

	target
}

fn merge_array(target: ArrayFilter, source: ArrayFilter) -> ArrayFilter {
	match (target, source) {
		(Some(target_filter), Some(source_filter)) => {
			Some(target_filter.merge(*source_filter).into())
		}

		_ => None,
	}
}

impl<'de> Deserialize<'de> for ColumnFilter {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let raw = String::deserialize(deserializer)?;

		let (remaining, filter) = group(&raw)
			.map_err(|error| de::Error::custom(format!("filter parse error: {error:?}")))?;

		if !remaining.is_empty() {
			return Err(de::Error::custom(
				"TODO: message. something broke and there's remaining characters.",
			));
		}

		Ok(filter)
	}
}

fn group(input: &str) -> IResult<&str, ColumnFilter> {
	map(
		separated_list1(tag(","), filter),
		// Unwrap is safe here, as `reduce` only returns `None` on 0-entry iterators, and `separated_list1` guarantees >=1 entries.
		|filters| filters.into_iter().reduce(|a, b| a.merge(b)).unwrap(),
	)(input)
}

fn filter(input: &str) -> IResult<&str, ColumnFilter> {
	alt((
		struct_entry,
		array_index,
		delimited(tag("("), group, tag(")")),
	))(input)
}

fn chained_filter(input: &str) -> IResult<&str, Option<ColumnFilter>> {
	opt(preceded(tag("."), filter))(input)
}

fn struct_entry(input: &str) -> IResult<&str, ColumnFilter> {
	map(tuple((field_name, chained_filter)), |(key, child)| {
		ColumnFilter::Struct(HashMap::from([(key.into(), child)]))
	})(input)
}

fn field_name(input: &str) -> IResult<&str, &str> {
	// TODO: ascii safe to use here? i'd hope?
	take_while1(|c: char| c.is_ascii_alphanumeric())(input)
}

fn array_index(input: &str) -> IResult<&str, ColumnFilter> {
	map(
		tuple((tag("[]"), chained_filter)),
		// TODO: actually parse an index
		|(_, child)| ColumnFilter::Array(child.map(Box::new)),
	)(input)
}

// TODO: need to add tests for error paths - and at that, add error handling. a lot of error cases (like mismatched types on a merge) can soft fail, but i should still surface warnings that they did soft fail. need to work out how that would work
#[cfg(test)]
mod test {
	use super::*;

	fn test_parse(input: &str) -> ColumnFilter {
		let (remaining, output) = group(input).expect("parse should not fail");
		assert_eq!(remaining, "");
		output
	}

	fn struct_filter(
		entries: impl IntoIterator<Item = (&'static str, Option<ColumnFilter>)>,
	) -> ColumnFilter {
		let map = entries
			.into_iter()
			.map(|(key, value)| (key.to_string(), value))
			.collect::<HashMap<_, _>>();
		ColumnFilter::Struct(map)
	}

	fn array_filter(child: Option<ColumnFilter>) -> ColumnFilter {
		ColumnFilter::Array(child.map(Box::new))
	}

	#[test]
	fn parse_struct_simple() {
		let out = test_parse("a");
		let expected = struct_filter([("a", None)]);
		assert_eq!(out, expected);
	}

	#[test]
	fn parse_struct_nested() {
		let out = test_parse("a.b");
		let expected = struct_filter([("a", Some(struct_filter([("b", None)])))]);
		assert_eq!(out, expected);
	}

	#[test]
	fn parse_array_simple() {
		let out = test_parse("[]");
		let expected = ColumnFilter::Array(None);
		assert_eq!(out, expected);
	}

	#[test]
	fn parse_array_nested() {
		let out = test_parse("a.[].[].b");
		let expected = struct_filter([(
			"a",
			Some(array_filter(Some(array_filter(Some(struct_filter([(
				"b", None,
			)])))))),
		)]);
		assert_eq!(out, expected);
	}

	// a,b -> {a, b}
	#[test]
	fn merge_struct_simple() {
		let out = test_parse("a,b");
		let expected = struct_filter([("a", None), ("b", None)]);
		assert_eq!(out, expected);
	}

	// a,a.b -> {a}
	#[test]
	fn merge_struct_widen() {
		let out = test_parse("a,a.b");
		let expected = struct_filter([("a", None)]);
		assert_eq!(out, expected);
	}

	// a.b,a.c -> {a: {b, c}}
	#[test]
	fn merge_struct_nested() {
		let out = test_parse("a.b,a.c");
		let expected = struct_filter([("a", Some(struct_filter([("b", None), ("c", None)])))]);
		assert_eq!(out, expected);
	}

	// a.(b,c),a.d -> {a: {b, c, d}}
	#[test]
	fn merge_nested_group() {
		let out = test_parse("a.(b,c),a.d");
		let expected = struct_filter([(
			"a",
			Some(struct_filter([("b", None), ("c", None), ("d", None)])),
		)]);
		assert_eq!(out, expected);
	}

	// [].a,[].b -> [{a, b}]
	#[test]
	fn merge_array_children() {
		let out = test_parse("[].a,[].b");
		let expected = array_filter(Some(struct_filter([("a", None), ("b", None)])));
		assert_eq!(out, expected);
	}
}
