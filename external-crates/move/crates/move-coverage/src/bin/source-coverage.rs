// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use clap::Parser;
use move_binary_format::file_format::CompiledModule;
use move_bytecode_source_map::utils::source_map_from_file;
use move_command_line_common::files::DEBUG_INFO_EXTENSION;
use move_coverage::{coverage_map::CoverageMap, source_coverage::SourceCoverageBuilder};
use std::{
    fs,
    fs::File,
    io::{self, Write},
    path::Path,
};

#[derive(Debug, Parser)]
#[clap(
    name = "source-coverage",
    about = "Annotate Move Source Code with Coverage Information",
    author,
    version
)]
struct Args {
    /// The path to the coverage map or trace file
    #[clap(long = "input-trace-path", short = 't')]
    pub input_trace_path: String,
    /// Whether the passed-in file is a raw trace file or a serialized coverage map
    #[clap(long = "is-raw-trace", short = 'r')]
    pub is_raw_trace_file: bool,
    /// The path to the module binary
    #[clap(long = "module-path", short = 'b')]
    pub module_binary_path: String,
    /// The path to the source file
    #[clap(long = "source-path", short = 's')]
    pub source_file_path: String,
    /// Optional path to save coverage. Printed to stdout if not present.
    #[clap(long = "coverage-path", short = 'o')]
    pub coverage_path: Option<String>,
}

fn main() {
    let args = Args::parse();
    let debug_info_extension = DEBUG_INFO_EXTENSION;
    let coverage_map = if args.is_raw_trace_file {
        CoverageMap::from_trace_file(&args.input_trace_path)
    } else {
        CoverageMap::from_binary_file(&args.input_trace_path).unwrap()
    };

    let bytecode_bytes = fs::read(&args.module_binary_path).expect("Unable to read bytecode file");
    let compiled_module = CompiledModule::deserialize_with_defaults(&bytecode_bytes)
        .expect("Module blob can't be deserialized");

    let source_map = source_map_from_file(
        &Path::new(&args.module_binary_path).with_extension(debug_info_extension),
    )
    .unwrap();
    let source_path = Path::new(&args.source_file_path);
    let source_cov = SourceCoverageBuilder::new(&compiled_module, &coverage_map, &source_map);

    let mut coverage_writer: Box<dyn Write> = match &args.coverage_path {
        Some(x) => {
            let path = Path::new(x);
            Box::new(File::create(path).unwrap())
        }
        None => Box::new(io::stdout()),
    };

    source_cov
        .compute_source_coverage(source_path)
        .output_source_coverage(&mut coverage_writer)
        .unwrap();
}
