use anyhow::{bail, Context, Ok, Result};
use indoc::formatdoc;
use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
    process::Command,
};
use tempdir::TempDir;
use uuid::Uuid;
pub mod app;
pub mod environment;
pub mod logger;
pub mod pkg;
pub mod plan;

use crate::providers::Provider;

use self::{
    app::App,
    environment::{Environment, EnvironmentVariables},
    logger::Logger,
    pkg::Pkg,
    plan::BuildPlan,
};

static NIX_PACKS_VERSION: &str = "0.0.1";

// https://status.nixos.org/
static NIXPKGS_ARCHIVE: &str = "30d3d79b7d3607d56546dd2a6b49e156ba0ec634";

#[derive(Debug)]
pub struct AppBuilderOptions {
    pub custom_build_cmd: Option<String>,
    pub custom_start_cmd: Option<String>,
    pub custom_pkgs: Vec<Pkg>,
    pub pin_pkgs: bool,
    pub out_dir: Option<String>,
    pub plan_path: Option<String>,
}

impl AppBuilderOptions {
    pub fn empty() -> AppBuilderOptions {
        AppBuilderOptions {
            custom_build_cmd: None,
            custom_start_cmd: None,
            custom_pkgs: Vec::new(),
            pin_pkgs: false,
            out_dir: None,
            plan_path: None,
        }
    }
}

pub struct AppBuilder<'a> {
    name: Option<String>,
    app: &'a App,
    environment: &'a Environment,
    logger: &'a Logger,
    options: &'a AppBuilderOptions,
    provider: Option<&'a dyn Provider>,
}

impl<'a> AppBuilder<'a> {
    pub fn new(
        name: Option<String>,
        app: &'a App,
        environment: &'a Environment,
        logger: &'a Logger,
        options: &'a AppBuilderOptions,
    ) -> Result<AppBuilder<'a>> {
        Ok(AppBuilder {
            name,
            app,
            environment,
            logger,
            options,
            provider: None,
        })
    }

    pub fn plan(&mut self, providers: Vec<&'a dyn Provider>) -> Result<BuildPlan> {
        // Load options from the best matching provider
        self.detect(providers).context("Detecting provider")?;

        let pkgs = self.get_pkgs().context("Getting packages")?;
        let install_cmd = self
            .get_install_cmd()
            .context("Generating install command")?;
        let build_cmd = self.get_build_cmd().context("Generating build command")?;
        let start_cmd = self.get_start_cmd().context("Generating start command")?;
        let variables = self.get_variables().context("Getting plan variables")?;

        let plan = BuildPlan {
            version: NIX_PACKS_VERSION.to_string(),
            nixpkgs_archive: if self.options.pin_pkgs {
                Some(NIXPKGS_ARCHIVE.to_string())
            } else {
                None
            },
            pkgs,
            install_cmd,
            start_cmd,
            build_cmd,
            variables,
        };

        Ok(plan)
    }

    pub fn build(&mut self, providers: Vec<&'a dyn Provider>) -> Result<()> {
        self.logger.log_section("Building");

        let plan = match &self.options.plan_path {
            Some(plan_path) => {
                self.logger.log_step("Building from existing plan");
                let plan_json = fs::read_to_string(plan_path).context("Reading build plan")?;
                let plan: BuildPlan =
                    serde_json::from_str(&plan_json).context("Deserializing build plan")?;
                plan
            }
            None => {
                self.logger.log_step("Generated new build plan");

                self.plan(providers).context("Creating build plan")?
            }
        };

        self.do_build(&plan)
    }

    pub fn do_build(&mut self, plan: &BuildPlan) -> Result<()> {
        let id = Uuid::new_v4();

        let dir: String = match &self.options.out_dir {
            Some(dir) => dir.clone(),
            None => {
                let tmp = TempDir::new("nixpacks").context("Creating a temp directory")?;
                let path = tmp.path().to_str().unwrap();
                path.to_string()
            }
        };

        self.logger.log_step("Copying source to tmp dir");

        let source = self.app.source.as_path().to_str().unwrap();
        let mut copy_cmd = Command::new("cp")
            .arg("-a")
            .arg(format!("{}/.", source))
            .arg(dir.clone())
            .spawn()?;
        let copy_result = copy_cmd.wait().context("Copying app source to tmp dir")?;
        if !copy_result.success() {
            bail!("Copy failed")
        }

        self.logger.log_step("Writing build plan");
        AppBuilder::write_build_plan(plan, dir.as_str()).context("Writing build plan")?;

        self.logger.log_step("Building image");

        let name = self.name.clone().unwrap_or_else(|| id.to_string());

        if self.options.out_dir.is_none() {
            let mut docker_build_cmd = Command::new("docker")
                .arg("build")
                .arg(dir)
                .arg("-t")
                .arg(name.clone())
                .spawn()?;

            let build_result = docker_build_cmd.wait().context("Building image")?;

            if !build_result.success() {
                bail!("Docker build failed")
            }

            self.logger.log_section("Successfully Built!");

            println!("\nRun:");
            println!("  docker run -it {}", name);
        } else {
            println!("\nSaved output to:");
            println!("  {}", dir);
        };

        Ok(())
    }

    fn get_pkgs(&self) -> Result<Vec<Pkg>> {
        let pkgs: Vec<Pkg> = match self.provider {
            Some(provider) => {
                let mut provider_pkgs = provider.pkgs(self.app, self.environment)?;
                let mut pkgs = self.options.custom_pkgs.clone();
                pkgs.append(&mut provider_pkgs);
                pkgs
            }
            None => self.options.custom_pkgs.clone(),
        };

        Ok(pkgs)
    }

    fn get_variables(&self) -> Result<EnvironmentVariables> {
        // Get a copy of the variables in the environment
        let variables = Environment::clone_variables(self.environment);

        let new_variables = match self.provider {
            Some(provider) => {
                // Merge provider variables
                let provider_variables =
                    provider.get_environment_variables(self.app, self.environment)?;
                provider_variables.into_iter().chain(variables).collect()
            }
            None => variables,
        };

        Ok(new_variables)
    }

    fn get_install_cmd(&self) -> Result<Option<String>> {
        let install_cmd = match self.provider {
            Some(provider) => provider.install_cmd(self.app, self.environment)?,
            None => None,
        };

        Ok(install_cmd)
    }

    fn get_build_cmd(&self) -> Result<Option<String>> {
        let suggested_build_cmd = match self.provider {
            Some(provider) => provider.suggested_build_cmd(self.app, self.environment)?,
            None => None,
        };

        let build_cmd = self
            .options
            .custom_build_cmd
            .clone()
            .or(suggested_build_cmd);

        Ok(build_cmd)
    }

    fn get_start_cmd(&self) -> Result<Option<String>> {
        let procfile_cmd = self.parse_procfile()?;

        let suggested_start_cmd = match self.provider {
            Some(provider) => provider.suggested_start_command(self.app, self.environment)?,
            None => None,
        };

        let start_cmd = self
            .options
            .custom_start_cmd
            .clone()
            .or(procfile_cmd)
            .or(suggested_start_cmd);

        Ok(start_cmd)
    }

    fn detect(&mut self, providers: Vec<&'a dyn Provider>) -> Result<()> {
        for provider in providers {
            let matches = provider.detect(self.app, self.environment)?;
            if matches {
                self.provider = Some(provider);
                break;
            }
        }

        Ok(())
    }

    fn parse_procfile(&self) -> Result<Option<String>> {
        if self.app.includes_file("Procfile") {
            let contents = self.app.read_file("Procfile")?;

            // Better error handling
            if contents.starts_with("web: ") {
                return Ok(Some(contents.replace("web: ", "").trim().to_string()));
            }

            Ok(None)
        } else {
            Ok(None)
        }
    }

    pub fn write_build_plan(plan: &BuildPlan, dest: &str) -> Result<()> {
        let nix_expression = AppBuilder::gen_nix(plan).context("Generating Nix expression")?;
        let dockerfile = AppBuilder::gen_dockerfile(plan).context("Generating Dockerfile")?;

        let nix_path = PathBuf::from(dest).join(PathBuf::from("environment.nix"));
        let mut nix_file = File::create(nix_path).context("Creating Nix environment file")?;
        nix_file
            .write_all(nix_expression.as_bytes())
            .context("Unable to write Nix expression")?;

        let dockerfile_path = PathBuf::from(dest).join(PathBuf::from("Dockerfile"));
        File::create(dockerfile_path.clone()).context("Creating Dockerfile file")?;
        fs::write(dockerfile_path, dockerfile).context("Writing Dockerfile")?;

        Ok(())
    }

    pub fn gen_nix(plan: &BuildPlan) -> Result<String> {
        let nixpkgs = plan
            .pkgs
            .iter()
            .map(|p| p.to_nix_string())
            .collect::<Vec<String>>()
            .join(" ");

        let nix_archive = plan.nixpkgs_archive.clone();
        let pkg_import = match nix_archive {
            Some(archive) => format!(
                "import (fetchTarball \"https://github.com/NixOS/nixpkgs/archive/{}.tar.gz\")",
                archive
            ),
            None => "import <nixpkgs>".to_string(),
        };

        let nix_expression = formatdoc! {"
            {{ }}:

            let
                pkgs = {pkg_import} {{ }};
            in with pkgs;
            buildEnv {{
                name = \"env\";
                paths = [
                    {pkgs}
                ];
            }}
        ",
        pkg_import=pkg_import,
        pkgs=nixpkgs};

        Ok(nix_expression)
    }

    pub fn gen_dockerfile(plan: &BuildPlan) -> Result<String> {
        let args_string = plan
            .variables
            .iter()
            .map(|var| format!("ENV {}='{}'", var.0, var.1))
            .collect::<Vec<String>>()
            .join("\n");

        let install_cmd = plan
            .install_cmd
            .as_ref()
            .map(|cmd| format!("RUN {}", cmd))
            .unwrap_or_else(|| "".to_string());
        let build_cmd = plan
            .build_cmd
            .as_ref()
            .map(|cmd| format!("RUN {}", cmd))
            .unwrap_or_else(|| "".to_string());
        let start_cmd = plan
            .start_cmd
            .as_ref()
            .map(|cmd| format!("CMD {}", cmd))
            .unwrap_or_else(|| "".to_string());

        let dockerfile = formatdoc! {"
          FROM nixos/nix

          RUN nix-channel --update

          RUN mkdir /app
          COPY environment.nix /app
          WORKDIR /app

          # Load Nix environment
          RUN nix-env -if environment.nix

          # Load environment variables
          {args_string}

          COPY . /app

          # Install
          {install_cmd}

          # Build
          {build_cmd}

          # Start
          {start_cmd}
        ",
        args_string=args_string,
        install_cmd=install_cmd,
        build_cmd=build_cmd,
        start_cmd=start_cmd};

        Ok(dockerfile)
    }
}
