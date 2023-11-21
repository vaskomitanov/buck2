/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Context as _;
use buck2_cli_proto::client_context::HostPlatformOverride;
use buck2_cli_proto::common_build_options::ExecutionStrategy;
use buck2_core::env_helper::EnvHelper;
use buck2_core::execution_types::executor_config::CacheUploadBehavior;
use buck2_core::execution_types::executor_config::CommandExecutorConfig;
use buck2_core::execution_types::executor_config::CommandGenerationOptions;
use buck2_core::execution_types::executor_config::Executor;
use buck2_core::execution_types::executor_config::HybridExecutionLevel;
use buck2_core::execution_types::executor_config::LocalExecutorOptions;
use buck2_core::execution_types::executor_config::PathSeparatorKind;
use buck2_core::execution_types::executor_config::RemoteEnabledExecutor;
use buck2_core::execution_types::executor_config::RemoteExecutorOptions;
use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::project::ProjectRoot;
use buck2_execute::execute::blocking::BlockingExecutor;
use buck2_execute::execute::cache_uploader::NoOpCacheUploader;
use buck2_execute::execute::dice_data::CommandExecutorResponse;
use buck2_execute::execute::dice_data::HasCommandExecutor;
use buck2_execute::execute::prepared::NoOpCommandOptionalExecutor;
use buck2_execute::execute::prepared::PreparedCommandExecutor;
use buck2_execute::execute::prepared::PreparedCommandOptionalExecutor;
use buck2_execute::execute::request::ExecutorPreference;
use buck2_execute::knobs::ExecutorGlobalKnobs;
use buck2_execute::materialize::materializer::Materializer;
use buck2_execute::re::manager::ReConnectionHandle;
use buck2_execute_impl::executors::action_cache::ActionCacheChecker;
use buck2_execute_impl::executors::action_cache::RemoteDepFileCacheChecker;
use buck2_execute_impl::executors::caching::CacheUploader;
use buck2_execute_impl::executors::hybrid::HybridExecutor;
use buck2_execute_impl::executors::local::LocalExecutor;
use buck2_execute_impl::executors::re::ReExecutor;
use buck2_execute_impl::executors::stacked::StackedExecutor;
use buck2_execute_impl::executors::worker::WorkerPool;
use buck2_execute_impl::low_pass_filter::LowPassFilter;
use buck2_execute_impl::re::paranoid_download::ParanoidDownloader;
use buck2_forkserver::client::ForkserverClient;
use dupe::Dupe;
use host_sharing::HostSharingBroker;
use remote_execution as RE;
use starlark_map::sorted_map::SortedMap;

pub fn parse_concurrency(requested: u32) -> anyhow::Result<usize> {
    let mut ret = requested.try_into().context("Invalid concurrency")?;

    if ret == 0 {
        ret = num_cpus::get();
    }

    Ok(ret)
}

/// For each buck invocations, we'll have a single CommandExecutorFactory. This contains shared
/// state used by all command executor strategies.
pub struct CommandExecutorFactory {
    pub re_connection: Arc<ReConnectionHandle>,
    // TODO(cjhopman): This should probably be a global limit, otherwise simultaneous commands may
    // use more resources than intended (this might no longer be accurate since only instances
    // sharing the same DICE context should be allowed to proceed concurrently, and we only have
    // one CommandExecutorFactory per DICE context).
    pub host_sharing_broker: Arc<HostSharingBroker>,
    pub low_pass_filter: Arc<LowPassFilter>,
    pub materializer: Arc<dyn Materializer>,
    pub blocking_executor: Arc<dyn BlockingExecutor>,
    pub strategy: ExecutionStrategy,
    pub executor_global_knobs: ExecutorGlobalKnobs,
    pub upload_all_actions: bool,
    pub forkserver: Option<ForkserverClient>,
    pub skip_cache_read: bool,
    pub skip_cache_write: bool,
    project_root: ProjectRoot,
    worker_pool: Arc<WorkerPool>,
    paranoid: Option<ParanoidDownloader>,
    materialize_failed_inputs: bool,
}

impl CommandExecutorFactory {
    pub fn new(
        re_connection: Arc<ReConnectionHandle>,
        host_sharing_broker: HostSharingBroker,
        low_pass_filter: LowPassFilter,
        materializer: Arc<dyn Materializer>,
        blocking_executor: Arc<dyn BlockingExecutor>,
        strategy: ExecutionStrategy,
        executor_global_knobs: ExecutorGlobalKnobs,
        upload_all_actions: bool,
        forkserver: Option<ForkserverClient>,
        skip_cache_read: bool,
        skip_cache_write: bool,
        project_root: ProjectRoot,
        worker_pool: Arc<WorkerPool>,
        paranoid: Option<ParanoidDownloader>,
        materialize_failed_inputs: bool,
    ) -> Self {
        Self {
            re_connection,
            host_sharing_broker: Arc::new(host_sharing_broker),
            low_pass_filter: Arc::new(low_pass_filter),
            materializer,
            blocking_executor,
            strategy,
            executor_global_knobs,
            upload_all_actions,
            forkserver,
            skip_cache_read,
            skip_cache_write,
            project_root,
            worker_pool,
            paranoid,
            materialize_failed_inputs,
        }
    }
}

impl HasCommandExecutor for CommandExecutorFactory {
    fn get_command_executor(
        &self,
        artifact_fs: &ArtifactFs,
        executor_config: &CommandExecutorConfig,
    ) -> anyhow::Result<CommandExecutorResponse> {
        // 30GB is the max RE can currently support.
        const DEFAULT_RE_MAX_INPUT_FILE_BYTES: u64 = 30 * 1024 * 1024 * 1024;

        let local_executor_new = |options: &LocalExecutorOptions| {
            let worker_pool = if options.use_persistent_workers {
                Some(self.worker_pool.dupe())
            } else {
                None
            };
            LocalExecutor::new(
                artifact_fs.clone(),
                self.materializer.dupe(),
                self.blocking_executor.dupe(),
                self.host_sharing_broker.dupe(),
                self.project_root.root().to_owned(),
                self.forkserver.dupe(),
                self.executor_global_knobs.dupe(),
                worker_pool,
            )
        };

        if !buck2_core::is_open_source() && !cfg!(fbcode_build) {
            static WARN: OnceLock<()> = OnceLock::new();
            WARN.get_or_init(|| {
                tracing::warn!("Cargo build detected: disabling remote execution and caching!")
            });

            if self.strategy.ban_local() {
                return Err(anyhow::anyhow!(
                    "The desired execution strategy (`{:?}`) is incompatible with the local executor",
                    self.strategy,
                ));
            }

            return Ok(CommandExecutorResponse {
                executor: Arc::new(local_executor_new(&LocalExecutorOptions::default())),
                platform: Default::default(),
                cache_checker: Arc::new(NoOpCommandOptionalExecutor {}),
                cache_uploader: Arc::new(NoOpCacheUploader {}),
            });
        }
        let remote_executor_new = |options: &RemoteExecutorOptions,
                                   re_use_case: &RemoteExecutorUseCase,
                                   re_action_key: &Option<String>,
                                   remote_cache_enabled: bool| {
            ReExecutor {
                artifact_fs: artifact_fs.clone(),
                project_fs: self.project_root.clone(),
                materializer: self.materializer.dupe(),
                re_client: self.re_connection.get_client(),
                re_use_case: *re_use_case,
                re_action_key: re_action_key.clone(),
                re_max_queue_time_ms: options.re_max_queue_time_ms,
                knobs: self.executor_global_knobs.dupe(),
                skip_cache_read: self.skip_cache_read || !remote_cache_enabled,
                skip_cache_write: self.skip_cache_write || !remote_cache_enabled,
                paranoid: self.paranoid.dupe(),
                materialize_failed_inputs: self.materialize_failed_inputs,
            }
        };

        let response = match &executor_config.executor {
            Executor::Local(local) => {
                if self.strategy.ban_local() {
                    None
                } else {
                    Some(CommandExecutorResponse {
                        executor: Arc::new(local_executor_new(local)),
                        platform: Default::default(),
                        cache_checker: Arc::new(NoOpCommandOptionalExecutor {}),
                        cache_uploader: Arc::new(NoOpCacheUploader {}),
                    })
                }
            }
            Executor::RemoteEnabled {
                executor,
                re_properties,
                re_use_case,
                re_action_key,
                cache_upload_behavior,
                remote_cache_enabled,
                remote_dep_file_cache_enabled,
            } => {
                // NOTE: While we now have a legit flag for this, we keep the env var. This has been used
                // in remediating prod incidents in the past, and this is the kind of thing that can easily
                // become tribal knowledge. Keeping this does not hurt us.
                static DISABLE_CACHING: EnvHelper<bool> =
                    EnvHelper::new("BUCK2_TEST_DISABLE_CACHING");

                let disable_caching = DISABLE_CACHING
                    .get_copied()?
                    .unwrap_or(self.skip_cache_read);

                let disable_caching =
                    disable_caching || (!remote_cache_enabled && !remote_dep_file_cache_enabled);

                // This is for test only as in real life, it would be silly to only use the remote dep file cache and not the regular cache
                // This will only do anything if cache is not disabled and remote dep file cache is enabled
                static ONLY_REMOTE_DEP_FILE_CACHE: EnvHelper<bool> =
                    EnvHelper::new("BUCK2_TEST_ONLY_REMOTE_DEP_FILE_CACHE");
                let only_remote_dep_file_cache =
                    ONLY_REMOTE_DEP_FILE_CACHE.get_copied()?.unwrap_or(false);

                let cache_checker_new = || -> Arc<dyn PreparedCommandOptionalExecutor> {
                    if disable_caching {
                        return Arc::new(NoOpCommandOptionalExecutor {}) as _;
                    }

                    let remote_dep_file_checker: Arc<dyn PreparedCommandOptionalExecutor> =
                        if *remote_dep_file_cache_enabled {
                            Arc::new(RemoteDepFileCacheChecker {
                                artifact_fs: artifact_fs.clone(),
                                materializer: self.materializer.dupe(),
                                re_client: self.re_connection.get_client(),
                                re_use_case: *re_use_case,
                                re_action_key: re_action_key.clone(),
                                upload_all_actions: self.upload_all_actions,
                                knobs: self.executor_global_knobs.dupe(),
                                paranoid: self.paranoid.dupe(),
                            }) as _
                        } else {
                            Arc::new(NoOpCommandOptionalExecutor {}) as _
                        };

                    if only_remote_dep_file_cache {
                        remote_dep_file_checker
                    } else {
                        Arc::new(ActionCacheChecker {
                            artifact_fs: artifact_fs.clone(),
                            materializer: self.materializer.dupe(),
                            re_client: self.re_connection.get_client(),
                            re_use_case: *re_use_case,
                            re_action_key: re_action_key.clone(),
                            upload_all_actions: self.upload_all_actions,
                            knobs: self.executor_global_knobs.dupe(),
                            paranoid: self.paranoid.dupe(),
                            remote_dep_file_checker,
                        }) as _
                    }
                };

                let executor: Option<Arc<dyn PreparedCommandExecutor>> = match &executor {
                    RemoteEnabledExecutor::Local(local) if !self.strategy.ban_local() => {
                        Some(Arc::new(local_executor_new(local)))
                    }
                    RemoteEnabledExecutor::Remote(remote) if !self.strategy.ban_remote() => {
                        Some(Arc::new(remote_executor_new(
                            remote,
                            re_use_case,
                            re_action_key,
                            *remote_cache_enabled,
                        )))
                    }
                    RemoteEnabledExecutor::Hybrid {
                        local,
                        remote,
                        level,
                    } if !self.strategy.ban_hybrid() => {
                        let re_max_input_files_bytes = remote
                            .re_max_input_files_bytes
                            .unwrap_or(DEFAULT_RE_MAX_INPUT_FILE_BYTES);
                        let local = local_executor_new(local);
                        let remote = remote_executor_new(
                            remote,
                            re_use_case,
                            re_action_key,
                            *remote_cache_enabled,
                        );
                        let executor_preference = self.strategy.hybrid_preference();
                        let low_pass_filter = self.low_pass_filter.dupe();

                        if self.paranoid.is_some() {
                            let executor_preference = executor_preference
                                .and(ExecutorPreference::DefaultErasePreferences)?;

                            Some(Arc::new(HybridExecutor {
                                local,
                                remote: StackedExecutor {
                                    optional: cache_checker_new(),
                                    fallback: remote,
                                },
                                level: HybridExecutionLevel::Full {
                                    fallback_on_failure: true,
                                    low_pass_filter: false,
                                },
                                executor_preference,
                                re_max_input_files_bytes,
                                low_pass_filter,
                            }))
                        } else {
                            Some(Arc::new(HybridExecutor {
                                local,
                                remote,
                                level: *level,
                                executor_preference,
                                re_max_input_files_bytes,
                                low_pass_filter,
                            }))
                        }
                    }
                    _ => None,
                };

                let cache_checker = if self.paranoid.is_some() {
                    Arc::new(NoOpCommandOptionalExecutor {}) as _
                } else {
                    cache_checker_new()
                };

                let platform = RE::Platform {
                    properties: re_properties
                        .iter()
                        .map(|(k, v)| RE::Property {
                            name: k.clone(),
                            value: v.clone(),
                        })
                        .collect(),
                };

                let cache_uploader = if disable_caching {
                    Arc::new(NoOpCacheUploader {}) as _
                } else if let CacheUploadBehavior::Enabled { max_bytes } = cache_upload_behavior {
                    Arc::new(CacheUploader::new(
                        artifact_fs.clone(),
                        self.materializer.dupe(),
                        self.re_connection.get_client(),
                        *re_use_case,
                        platform.clone(),
                        *max_bytes,
                    )) as _
                } else {
                    Arc::new(NoOpCacheUploader {}) as _
                };

                executor.map(|executor| CommandExecutorResponse {
                    executor,
                    platform,
                    cache_checker,
                    cache_uploader,
                })
            }
        };

        let response = response
            .with_context(|| format!(
"The desired execution strategy (`{:?}`) is incompatible with the executor config that was selected: {:?}",
self.strategy, executor_config))?;

        Ok(response)
    }
}

trait ExecutionStrategyExt {
    fn ban_local(&self) -> bool;
    fn ban_remote(&self) -> bool;
    fn ban_hybrid(&self) -> bool;
    fn hybrid_preference(&self) -> ExecutorPreference;
}

impl ExecutionStrategyExt for ExecutionStrategy {
    fn ban_local(&self) -> bool {
        match self {
            Self::RemoteOnly | Self::NoExecution => true,
            _ => false,
        }
    }

    fn ban_remote(&self) -> bool {
        match self {
            Self::LocalOnly | Self::NoExecution => true,
            _ => false,
        }
    }

    fn ban_hybrid(&self) -> bool {
        match self {
            Self::NoExecution => true,
            _ => false,
        }
    }

    fn hybrid_preference(&self) -> ExecutorPreference {
        match self {
            Self::HybridPreferLocal => ExecutorPreference::LocalPreferred,
            Self::HybridPreferRemote => ExecutorPreference::RemotePreferred,
            Self::LocalOnly => ExecutorPreference::LocalRequired,
            Self::RemoteOnly => ExecutorPreference::RemoteRequired,
            _ => ExecutorPreference::Default,
        }
    }
}

/// This is used when execution platforms are not configured.
pub fn get_default_executor_config(host_platform: HostPlatformOverride) -> CommandExecutorConfig {
    let executor = if buck2_core::is_open_source() {
        Executor::Local(LocalExecutorOptions::default())
    } else {
        Executor::RemoteEnabled {
            executor: RemoteEnabledExecutor::Hybrid {
                local: LocalExecutorOptions::default(),
                remote: RemoteExecutorOptions::default(),
                level: HybridExecutionLevel::Limited,
            },
            re_properties: get_default_re_properties(host_platform),
            re_use_case: RemoteExecutorUseCase::buck2_default(),
            re_action_key: None,
            cache_upload_behavior: CacheUploadBehavior::Disabled,
            remote_cache_enabled: true,
            remote_dep_file_cache_enabled: false,
        }
    };

    CommandExecutorConfig {
        executor,
        options: CommandGenerationOptions {
            path_separator: get_default_path_separator(host_platform),
            output_paths_behavior: Default::default(),
        },
    }
}

fn get_default_re_properties(host_platform: HostPlatformOverride) -> SortedMap<String, String> {
    let linux = &[("platform", "linux-remote-execution")];
    let macos = &[("platform", "mac"), ("subplatform", "any")];
    let windows = &[("platform", "windows")];

    let props = match host_platform {
        HostPlatformOverride::Linux => linux.as_slice(),
        HostPlatformOverride::MacOs => macos.as_slice(),
        HostPlatformOverride::Windows => windows.as_slice(),
        HostPlatformOverride::DefaultPlatform => match std::env::consts::OS {
            "linux" => linux.as_slice(),
            "macos" => macos.as_slice(),
            "windows" => windows.as_slice(),
            v => unimplemented!("no support yet for operating system `{}`", v),
        },
    };

    props
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn get_default_path_separator(host_platform: HostPlatformOverride) -> PathSeparatorKind {
    match host_platform {
        HostPlatformOverride::Linux => PathSeparatorKind::Unix,
        HostPlatformOverride::MacOs => PathSeparatorKind::Unix,
        HostPlatformOverride::Windows => PathSeparatorKind::Windows,
        HostPlatformOverride::DefaultPlatform => PathSeparatorKind::system_default(),
    }
}
