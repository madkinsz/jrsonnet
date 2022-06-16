use std::borrow::Cow;

use jrsonnet_parser::{LocExpr, ParserSettings, Source};

thread_local! {
	/// To avoid parsing again when issued from the same thread
	#[allow(unreachable_code)]
	static PARSED_STDLIB: LocExpr = {
		#[cfg(feature = "serialized-stdlib")]
		{
			// Should not panic, stdlib.bincode is generated in build.rs
			return bincode::deserialize(include_bytes!(concat!(env!("OUT_DIR"), "/stdlib.bincode")))
				.unwrap();
		}

		jrsonnet_parser::parse(
			jrsonnet_stdlib::STDLIB_STR,
			&ParserSettings {
				file_name: Source::new_virtual(Cow::Borrowed("<std>")),
			},
		)
		.unwrap()
	}
}

pub fn get_parsed_stdlib() -> LocExpr {
	PARSED_STDLIB.with(Clone::clone)
}
