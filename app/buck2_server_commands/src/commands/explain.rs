/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_cli_proto::new_generic::ExplainRequest;
use buck2_cli_proto::new_generic::ExplainResponse;
use buck2_core::fs::paths::abs_path::AbsPathBuf;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::partial_result_dispatcher::NoPartialResult;
use buck2_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use buck2_server_ctx::template::run_server_command;
use buck2_server_ctx::template::ServerCommandTemplate;
use dice::DiceTransaction;
use tonic::async_trait;

pub(crate) async fn explain_command(
    ctx: &dyn ServerCommandContextTrait,
    partial_result_dispatcher: PartialResultDispatcher<NoPartialResult>,
    req: ExplainRequest,
) -> anyhow::Result<ExplainResponse> {
    run_server_command(
        ExplainServerCommand {
            output: req.output,
            target: req.target,
        },
        ctx,
        partial_result_dispatcher,
    )
    .await
}
struct ExplainServerCommand {
    output: AbsPathBuf,
    target: String,
}

#[async_trait]
impl ServerCommandTemplate for ExplainServerCommand {
    type StartEvent = buck2_data::ExplainCommandStart;
    type EndEvent = buck2_data::ExplainCommandEnd;
    type Response = buck2_cli_proto::new_generic::ExplainResponse;
    type PartialResult = NoPartialResult;

    async fn command(
        &self,
        server_ctx: &dyn ServerCommandContextTrait,
        _partial_result_dispatcher: PartialResultDispatcher<Self::PartialResult>,
        ctx: DiceTransaction,
    ) -> anyhow::Result<Self::Response> {
        explain(server_ctx, ctx, &self.output, &self.target).await
    }

    fn is_success(&self, _response: &Self::Response) -> bool {
        // No response if we failed.
        true
    }

    fn exclusive_command_name(&self) -> Option<String> {
        Some("explain".to_owned())
    }
}

pub(crate) async fn explain(
    _server_ctx: &dyn ServerCommandContextTrait,
    mut _ctx: DiceTransaction,
    destination_path: &AbsPathBuf,
    _target: &str,
) -> anyhow::Result<ExplainResponse> {
    // TODO iguridi: make it work for OSS
    #[cfg(fbcode_build)]
    {
        // TODO iguridi: get the target graph from target without using cquery
        let base64 = base64::encode("temporary placeholder");
        // write the output to html
        buck2_explain::main(base64, destination_path)?;
    }
    #[cfg(not(fbcode_build))]
    {
        // just "using" unused variable
        let _destination_path = destination_path;
    }

    Ok(ExplainResponse {})
}
