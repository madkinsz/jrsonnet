use std::{borrow::Cow, env, fs::File, io::Write, path::Path};

use bincode::serialize;
use jrsonnet_parser::{parse, ParserSettings, Source};
use jrsonnet_stdlib::STDLIB_STR;

fn main() {
	let parsed = parse(
		STDLIB_STR,
		&ParserSettings {
			file_name: Source::new_virtual(Cow::Borrowed("<std>")),
		},
	)
	.expect("parse");

	{
		let out_dir = env::var("OUT_DIR").unwrap();
		let dest_path = Path::new(&out_dir).join("stdlib.bincode");
		let mut f = File::create(&dest_path).unwrap();
		f.write_all(&serialize(&parsed).unwrap()).unwrap();
	}
}
