use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use bb::AppBuilder;
use builders::{Builder, NpmBuilder, YarnBuilder};
use clap::{arg, Arg, Command};
mod bb;
mod builders;

fn main() -> Result<()> {
    let matches = Command::new("bb")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("build")
                .about("Create a Docker build-able directory from app source")
                .arg(arg!(<PATH> "App source"))
                .arg(
                    Arg::new("build_cmd")
                        .long("build-cmd")
                        .short('b')
                        .help("Specify the build command to use")
                        .takes_value(true),
                )
                .arg(
                    Arg::new("start_cmd")
                        .long("start-cmd")
                        .short('s')
                        .help("Specify the start command to use")
                        .takes_value(true),
                ),
        )
        .get_matches();

    match &matches.subcommand() {
        Some(("build", query_matches)) => {
            let path = query_matches.value_of("PATH").expect("required");
            let build_cmd = query_matches.value_of("build_cmd").map(|s| s.to_string());
            let start_cmd = query_matches.value_of("start_cmd").map(|s| s.to_string());

            let builders: Vec<Box<dyn Builder>> =
                vec![Box::new(YarnBuilder {}), Box::new(NpmBuilder {})];

            let source = fs::canonicalize(PathBuf::from(path.to_string()))
                .context("Invalid app source directory")?;

            let mut app_builder = AppBuilder::new(source, build_cmd, start_cmd);
            app_builder.detect(&builders)?;

            app_builder.build()?;
        }
        _ => unreachable!(),
    }

    Ok(())
}
