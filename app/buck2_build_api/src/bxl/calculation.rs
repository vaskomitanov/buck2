/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! DICE calculations for bxl

use std::fmt::Debug;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_artifact::artifact::build_artifact::BuildArtifact;
use buck2_core::base_deferred_key::BaseDeferredKeyDyn;
use buck2_util::late_binding::LateBinding;
use dashmap::DashMap;
use dice::DiceComputations;
use dupe::Dupe;

use crate::bxl::result::BxlResult;

#[async_trait]
pub trait BxlCalculationDyn: Debug + Send + Sync + 'static {
    async fn eval_bxl(
        &self,
        ctx: &DiceComputations,
        bxl: Arc<dyn BaseDeferredKeyDyn>,
    ) -> anyhow::Result<BxlComputeResult>;
}

#[derive(Allocative, Clone, Dupe)]
pub struct BxlComputeResult {
    pub bxl_result: Arc<BxlResult>,
    pub materializations: Arc<DashMap<BuildArtifact, ()>>,
}

/// Dependency injection for BXL.
///
/// BXL implementation lives in downstream crate.
/// This field is initialized at program start, so this crate can call BXL calculation.
pub static BXL_CALCULATION_IMPL: LateBinding<&'static dyn BxlCalculationDyn> =
    LateBinding::new("BXL_CALCULATION_IMPL");
