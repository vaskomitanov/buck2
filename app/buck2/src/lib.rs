/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

#![feature(error_generic_member_access)]
#![feature(used_with_arg)]

use anyhow::Context as _;
use buck2_audit::AuditCommand;
use buck2_client::commands::build::BuildCommand;
use buck2_client::commands::bxl::BxlCommand;
use buck2_client::commands::clean::CleanCommand;
use buck2_client::commands::complete::CompleteCommand;
use buck2_client::commands::completion::CompletionCommand;
use buck2_client::commands::ctargets::ConfiguredTargetsCommand;
use buck2_client::commands::debug::DebugCommand;
use buck2_client::commands::expand_external_cell::ExpandExternalCellCommand;
use buck2_client::commands::explain::ExplainCommand;
use buck2_client::commands::help_env::HelpEnvCommand;
use buck2_client::commands::init::InitCommand;
use buck2_client::commands::install::InstallCommand;
use buck2_client::commands::kill::KillCommand;
use buck2_client::commands::killall::KillallCommand;
use buck2_client::commands::log::LogCommand;
use buck2_client::commands::lsp::LspCommand;
use buck2_client::commands::profile::ProfileCommand;
use buck2_client::commands::query::aquery::AqueryCommand;
use buck2_client::commands::query::cquery::CqueryCommand;
use buck2_client::commands::query::uquery::UqueryCommand;
use buck2_client::commands::rage::RageCommand;
use buck2_client::commands::root::RootCommand;
use buck2_client::commands::run::RunCommand;
use buck2_client::commands::server::ServerCommand;
use buck2_client::commands::status::StatusCommand;
use buck2_client::commands::subscribe::SubscribeCommand;
use buck2_client::commands::targets::TargetsCommand;
use buck2_client::commands::test::TestCommand;
use buck2_client_ctx::argfiles::expand_argfiles_with_context;
use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::client_metadata::ClientMetadata;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::immediate_config::ImmediateConfigContext;
use buck2_client_ctx::streaming::BuckSubcommand;
use buck2_client_ctx::tokio_runtime_setup::client_tokio_runtime;
use buck2_client_ctx::version::BuckVersion;
use buck2_common::argv::Argv;
use buck2_common::invocation_paths::InvocationPaths;
use buck2_common::invocation_roots::find_invocation_roots;
use buck2_core::buck2_env;
use buck2_core::fs::paths::file_name::FileNameBuf;
use buck2_event_observer::verbosity::Verbosity;
use buck2_starlark::StarlarkCommand;
use buck2_util::cleanup_ctx::AsyncCleanupContextGuard;
use clap::CommandFactory;
use clap::FromArgMatches;
use dupe::Dupe;

use crate::check_user_allowed::check_user_allowed;
use crate::commands::docs::DocsCommand;
use crate::commands::forkserver::ForkserverCommand;
use crate::process_context::ProcessContext;

mod check_user_allowed;
mod cli_style;
pub(crate) mod commands;
#[cfg(not(client_only))]
mod no_buckd;
pub mod panic;
pub mod process_context;

fn parse_isolation_dir(s: &str) -> anyhow::Result<FileNameBuf> {
    FileNameBuf::try_from(s.to_owned()).context("isolation dir must be a directory name")
}

/// Options of `buck2` command, before subcommand.
#[derive(Clone, Debug, clap::Parser)]
#[clap(next_help_heading = "Universal Options")]
struct BeforeSubcommandOptions {
    /// The name of the directory that Buck2 creates within buck-out for writing outputs and daemon
    /// information. If one is not provided, Buck2 creates a directory with the default name.
    ///
    /// Instances of Buck2 share a daemon if and only if their isolation directory is identical.
    /// The isolation directory also influences the output paths provided by Buck2,
    /// and as a result using a non-default isolation dir will cause cache misses (and slower builds).
    #[clap(
        value_parser = parse_isolation_dir,
        env("BUCK_ISOLATION_DIR"),
        long,
        default_value="v2"
    )]
    isolation_dir: FileNameBuf,

    /// How verbose buck should be while logging.
    ///
    /// Values:
    /// 0 = Quiet, errors only;
    /// 1 = Show status. Default;
    /// 2 = more info about errors;
    /// 3 = more info about everything;
    /// 4 = more info about everything + stderr;
    ///
    /// It can be combined with specific log items (stderr, full_failed_command, commands, actions,
    /// status, stats, success) to fine-tune the verbosity of the log. Example usage "-v=1,stderr"
    #[clap(
        short = 'v',
        long = "verbose",
        default_value = "1",
        global = true,
        value_parser= Verbosity::try_from_cli
    )]
    verbosity: Verbosity,

    /// The oncall executing this command
    #[clap(long, global = true)]
    oncall: Option<String>,

    /// Metadata key-value pairs to inject into Buck2's logging. Client metadata must be of the
    /// form `key=value`, where `key` is a snake_case identifier, and will be sent to backend
    /// datasets.
    #[clap(long, global = true)]
    client_metadata: Vec<ClientMetadata>,

    /// Do not launch a daemon process, run buck server in client process.
    ///
    /// Note even when running in no-buckd mode, it still writes state files.
    /// In particular, this command effectively kills buckd process
    /// running with the same isolation directory.
    ///
    /// This is an unsupported option used only for development work.
    #[clap(env("BUCK2_NO_BUCKD"), long, global(true), hide(true))]
    // Env var is BUCK2_NO_BUCKD instead of NO_BUCKD env var from buck1 because no buckd
    // is not supported for production work for buck2 and lots of places already set
    // NO_BUCKD=1 for buck1.
    no_buckd: bool,

    /// Print buck wrapper help.
    #[clap(skip)] // @oss-enable
    // @oss-disable: #[clap(long)]
    help_wrapper: bool,
}

#[rustfmt::skip] // Formatting in internal and in OSS versions disagree after oss markers applied.
fn help() -> &'static str {
    concat!(
        "A build system\n",
        "\n",
        "Documentation: https://buck2.build/docs/\n", // @oss-enable
        // @oss-disable: "Documentation: https://internalfb.com/intern/staticdocs/buck2/docs/\n",
    )
}

#[derive(Debug, clap::Parser)]
#[clap(
    name = "buck2",
    about(Some(help())),
    version(BuckVersion::get_version()),
    styles = cli_style::get_styles(),
)]
pub(crate) struct Opt {
    #[clap(subcommand)]
    cmd: CommandKind,
    #[clap(flatten)]
    common_opts: BeforeSubcommandOptions,
}

impl Opt {
    pub(crate) fn exec(
        self,
        process: ProcessContext<'_>,
        immediate_config: &ImmediateConfigContext,
        matches: &clap::ArgMatches,
        argv: Argv,
    ) -> ExitResult {
        let subcommand_matches = match matches.subcommand().map(|s| s.1) {
            Some(submatches) => submatches,
            None => panic!("Parsed a subcommand but couldn't extract subcommand argument matches"),
        };

        self.cmd.exec(
            process,
            immediate_config,
            subcommand_matches,
            argv,
            self.common_opts,
        )
    }
}

pub fn exec(process: ProcessContext<'_>) -> ExitResult {
    let mut immediate_config = ImmediateConfigContext::new(process.working_dir);
    let mut expanded_args =
        expand_argfiles_with_context(process.args.to_vec(), &mut immediate_config)
            .context("Error expanding argsfiles")?;

    // Override arg0 in `buck2 help`.
    if let Some(arg0) = buck2_env!("BUCK2_ARG0")? {
        expanded_args[0] = arg0.to_owned();
    }

    let clap = Opt::command();
    let matches = clap.get_matches_from(&expanded_args);
    let opt: Opt = Opt::from_arg_matches(&matches)?;

    if opt.common_opts.help_wrapper {
        return ExitResult::err(anyhow::anyhow!(
            "`--help-wrapper` should have been handled by the wrapper"
        ));
    }

    match &opt.cmd {
        #[cfg(not(client_only))]
        CommandKind::Daemon(..) => {}
        CommandKind::Clean(..) | CommandKind::Forkserver(..) => {}
        _ => {
            check_user_allowed()?;
        }
    }

    let argv = Argv {
        argv: process.args.to_vec(),
        expanded_argv: expanded_args,
    };

    opt.exec(process, &immediate_config, &matches, argv)
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum CommandKind {
    #[cfg(not(client_only))]
    #[clap(hide = true)]
    Daemon(crate::commands::daemon::DaemonCommand),
    #[clap(hide = true)]
    Forkserver(ForkserverCommand),
    #[cfg(not(client_only))]
    #[clap(hide = true)]
    InternalTestRunner(crate::commands::internal_test_runner::InternalTestRunnerCommand),
    #[clap(subcommand)]
    Audit(AuditCommand),
    Aquery(AqueryCommand),
    Build(BuildCommand),
    Bxl(BxlCommand),
    // TODO(nga): implement `buck2 help-buckconfig` too
    //   https://www.internalfb.com/tasks/?t=183528129
    HelpEnv(HelpEnvCommand),
    Test(TestCommand),
    Cquery(CqueryCommand),
    Init(InitCommand),
    #[clap(hide = true)] // TODO iguridi: remove
    Explain(ExplainCommand),
    ExpandExternalCell(ExpandExternalCellCommand),
    Install(InstallCommand),
    Kill(KillCommand),
    Killall(KillallCommand),
    Root(RootCommand),
    /// Alias for `uquery`.
    Query(UqueryCommand),
    Run(RunCommand),
    Server(ServerCommand),
    Status(StatusCommand),
    #[clap(subcommand)]
    Starlark(StarlarkCommand),
    /// Alias for `utargets`.
    Targets(TargetsCommand),
    Utargets(TargetsCommand),
    Ctargets(ConfiguredTargetsCommand),
    Uquery(UqueryCommand),
    #[clap(subcommand, hide = true)]
    Debug(DebugCommand),
    #[clap(hide = true)]
    Complete(CompleteCommand),
    Completion(CompletionCommand),
    Docs(DocsCommand),
    #[clap(subcommand)]
    Profile(ProfileCommand),
    #[clap(hide(true))] // @oss-enable
    Rage(RageCommand),
    Clean(CleanCommand),
    #[clap(subcommand)]
    Log(LogCommand),
    Lsp(LspCommand),
    Subscribe(SubscribeCommand),
}

impl CommandKind {
    pub(crate) fn exec(
        self,
        process: ProcessContext<'_>,
        immediate_config: &ImmediateConfigContext,
        matches: &clap::ArgMatches,
        argv: Argv,
        common_opts: BeforeSubcommandOptions,
    ) -> ExitResult {
        let roots = find_invocation_roots(process.working_dir.path());
        let paths = roots
            .map(|r| InvocationPaths {
                roots: r,
                isolation: common_opts.isolation_dir.clone(),
            })
            .map_err(buck2_error::Error::from);

        // Handle the daemon command earlier: it wants to fork, but the things we do below might
        // want to create threads.
        #[cfg(not(client_only))]
        if let CommandKind::Daemon(cmd) = self {
            return cmd
                .exec(
                    process.init,
                    process.log_reload_handle.dupe(),
                    paths?,
                    false,
                    || {},
                )
                .into();
        }

        if common_opts.no_buckd {
            // `no_buckd` can't work in a client-only binary
            if let Some(res) = ExitResult::retry_command_with_full_binary()? {
                return res;
            }
        }

        let runtime = client_tokio_runtime()?;
        let async_cleanup = AsyncCleanupContextGuard::new(&runtime);

        let start_in_process_daemon = if common_opts.no_buckd {
            #[cfg(not(client_only))]
            let v = no_buckd::start_in_process_daemon(
                process.init,
                immediate_config.daemon_startup_config()?,
                paths.clone()?,
                &runtime,
            )?;
            #[cfg(client_only)]
            let v = unreachable!(); // case covered above
            #[allow(dead_code)]
            v
        } else {
            None
        };

        let command_ctx = ClientCommandContext::new(
            process.init,
            immediate_config,
            paths,
            process.working_dir.clone(),
            common_opts.verbosity,
            start_in_process_daemon,
            argv,
            process.trace_id.dupe(),
            async_cleanup.ctx().dupe(),
            process.stdin,
            process.restarter,
            process.restarted_trace_id.dupe(),
            &runtime,
            common_opts.oncall,
            common_opts.client_metadata,
        );

        match self {
            #[cfg(not(client_only))]
            CommandKind::Daemon(..) => unreachable!("Checked earlier"),
            CommandKind::Forkserver(cmd) => cmd
                .exec(matches, command_ctx, process.log_reload_handle.dupe())
                .into(),
            #[cfg(not(client_only))]
            CommandKind::InternalTestRunner(cmd) => cmd.exec(matches, command_ctx).into(),
            CommandKind::Aquery(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Build(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Bxl(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Test(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Cquery(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::HelpEnv(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Kill(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Killall(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Clean(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Root(cmd) => cmd.exec(matches, command_ctx).into(),
            CommandKind::Query(cmd) => {
                buck2_client_ctx::eprintln!(
                    "WARNING: \"buck2 query\" is an alias for \"buck2 uquery\". Consider using \"buck2 cquery\" or \"buck2 uquery\" explicitly."
                )?;
                cmd.exec(matches, command_ctx)
            }
            CommandKind::Server(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Status(cmd) => cmd.exec(matches, command_ctx).into(),
            CommandKind::Targets(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Utargets(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Ctargets(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Audit(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Starlark(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Run(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Uquery(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Debug(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Complete(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Completion(cmd) => cmd.exec(Opt::command(), matches, command_ctx),
            CommandKind::Docs(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Profile(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Rage(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Init(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Explain(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Install(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Log(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Lsp(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Subscribe(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::ExpandExternalCell(cmd) => cmd.exec(matches, command_ctx),
        }
    }
}
