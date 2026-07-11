//! Hand-written prost types for the zccache `rust-plan` protobuf schema.
//!
//! The schema file lives next to this module as `rust_plan_manifest.proto`.
//! Keep the two in sync with zccache's `zccache.rust_plan.v1` schema.

use prost::Message as _;

use crate::core::SoldrError;

use super::RustArtifactPlan;

pub mod wire {
    use prost::Message;

    #[derive(Clone, PartialEq, Message)]
    pub struct RustArtifactPlanV1 {
        #[prost(uint32, tag = "1")]
        pub schema_version: u32,
        #[prost(uint32, tag = "2")]
        pub mode: u32,
        #[prost(string, tag = "3")]
        pub workspace_root: String,
        #[prost(string, tag = "4")]
        pub target_dir: String,
        #[prost(message, optional, tag = "5")]
        pub toolchain: Option<RustToolchainIdentity>,
        #[prost(string, tag = "6")]
        pub target_triple: String,
        #[prost(string, tag = "7")]
        pub profile: String,
        #[prost(message, optional, tag = "8")]
        pub inputs: Option<RustPlanInputs>,
        #[prost(message, optional, tag = "9")]
        pub packages: Option<RustPlanPackages>,
        #[prost(uint32, repeated, tag = "10")]
        pub allowed_artifact_classes: Vec<u32>,
        #[prost(uint32, tag = "11")]
        pub cache_schema_version: u32,
        #[prost(string, tag = "12")]
        pub journal_log_path: String,
        #[prost(string, tag = "13")]
        pub cache_profile: String,
        #[prost(uint32, repeated, tag = "14")]
        pub dropped_artifact_classes: Vec<u32>,
        #[prost(string, repeated, tag = "15")]
        pub cargo_artifact_paths: Vec<String>,
        #[prost(bool, tag = "16")]
        pub cargo_artifacts_complete: bool,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct RustToolchainIdentity {
        #[prost(string, tag = "1")]
        pub rustc: String,
        #[prost(string, tag = "2")]
        pub cargo: String,
        #[prost(string, tag = "3")]
        pub channel: String,
        #[prost(string, tag = "4")]
        pub host: String,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct RustPlanInputs {
        #[prost(string, tag = "1")]
        pub features_hash: String,
        #[prost(string, tag = "2")]
        pub rustflags_hash: String,
        #[prost(string, tag = "3")]
        pub env_hash: String,
        #[prost(string, tag = "4")]
        pub lockfile_hash: String,
        #[prost(string, tag = "5")]
        pub cargo_config_hash: String,
        #[prost(string, repeated, tag = "6")]
        pub manifest_hashes: Vec<String>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct RustPlanPackages {
        #[prost(string, repeated, tag = "1")]
        pub selected_package_ids: Vec<String>,
        #[prost(string, repeated, tag = "2")]
        pub workspace_package_ids: Vec<String>,
        #[prost(string, repeated, tag = "3")]
        pub excluded_path_package_ids: Vec<String>,
    }
}

pub(super) fn plan_to_proto_bytes(plan: &RustArtifactPlan) -> Result<Vec<u8>, SoldrError> {
    let proto = wire::RustArtifactPlanV1 {
        schema_version: plan.schema_version,
        mode: plan_mode_to_proto(&plan.mode)?,
        workspace_root: plan.workspace_root.clone(),
        target_dir: plan.target_dir.clone(),
        toolchain: Some(wire::RustToolchainIdentity {
            rustc: plan.toolchain.rustc.clone(),
            cargo: plan.toolchain.cargo.clone(),
            channel: plan.toolchain.channel.clone(),
            host: plan.toolchain.host.clone(),
        }),
        target_triple: plan.target_triple.clone(),
        profile: plan.profile.clone(),
        inputs: Some(wire::RustPlanInputs {
            features_hash: plan.inputs.features_hash.clone(),
            rustflags_hash: plan.inputs.rustflags_hash.clone(),
            env_hash: plan.inputs.env_hash.clone(),
            lockfile_hash: plan.inputs.lockfile_hash.clone(),
            cargo_config_hash: plan.inputs.cargo_config_hash.clone(),
            manifest_hashes: plan.inputs.manifest_hashes.clone(),
        }),
        packages: Some(wire::RustPlanPackages {
            selected_package_ids: plan.packages.selected_package_ids.clone(),
            workspace_package_ids: plan.packages.workspace_package_ids.clone(),
            excluded_path_package_ids: plan.packages.excluded_path_package_ids.clone(),
        }),
        allowed_artifact_classes: plan
            .allowed_artifact_classes
            .iter()
            .map(artifact_class_to_proto)
            .collect::<Result<Vec<_>, _>>()?,
        cache_schema_version: plan.cache_schema_version,
        journal_log_path: plan.journal_log_path.clone().unwrap_or_default(),
        cache_profile: plan.cache_profile.unwrap_or_default().to_string(),
        dropped_artifact_classes: plan
            .dropped_artifact_classes
            .iter()
            .map(artifact_class_to_proto)
            .collect::<Result<Vec<_>, _>>()?,
        cargo_artifact_paths: plan.cargo_artifact_paths.clone(),
        cargo_artifacts_complete: plan.cargo_artifacts_complete,
    };
    let mut bytes = Vec::with_capacity(proto.encoded_len());
    proto
        .encode(&mut bytes)
        .map_err(|err| SoldrError::Other(format!("failed to encode Rust artifact plan: {err}")))?;
    Ok(bytes)
}

fn plan_mode_to_proto(mode: &str) -> Result<u32, SoldrError> {
    match mode {
        "thin" => Ok(1),
        "full" => Ok(2),
        other => Err(SoldrError::Other(format!(
            "invalid Rust artifact plan mode {other:?}"
        ))),
    }
}

fn artifact_class_to_proto(class: &&'static str) -> Result<u32, SoldrError> {
    match *class {
        "rlib" => Ok(1),
        "rmeta" => Ok(2),
        "dep_info" => Ok(3),
        "proc_macro" => Ok(4),
        "shared_lib" => Ok(5),
        "cargo_fingerprint" => Ok(6),
        "cargo_fingerprint_meta" => Ok(7),
        "cargo_fingerprint_outputs" => Ok(8),
        "build_script_metadata" => Ok(9),
        "build_script_output" => Ok(10),
        "build_script_build" => Ok(11),
        "incremental" => Ok(12),
        "dwo" => Ok(13),
        "pdb" => Ok(14),
        "dsym" => Ok(15),
        "full_target" => Ok(16),
        other => Err(SoldrError::Other(format!(
            "invalid Rust artifact class {other:?}"
        ))),
    }
}
