//! Copy-dir asset pipeline.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::stream::{self, BoxStream};
use futures_util::StreamExt;
use nipper::Document;
use tokio::fs;

// #[cfg(test)]
// mod tests;
use super::{Asset, Output};
use crate::util::{
    copy_dir_recursive, trunk_id_selector, AssetInput, Error, ErrorReason, Result, ResultExt,
    ATTR_HREF, ATTR_REL,
};

static TYPE_COPY_DIR: &str = "copy-dir";

#[derive(Debug)]
struct Input {
    asset_input: AssetInput,
    /// The path to the dir being copied.
    path: PathBuf,
    /// Optional target path inside the dist dir.
    target_path: Option<PathBuf>,
}

impl TryFrom<AssetInput> for Input {
    type Error = Error;

    fn try_from(value: AssetInput) -> std::result::Result<Self, Self::Error> {
        if value.attrs.get(ATTR_REL).map(|m| m.as_str()) != Some(TYPE_COPY_DIR) {
            return Err(ErrorReason::AssetNotMatched { input: value }.into_error());
        }

        // Build the path to the target asset.
        let href_attr =
            value
                .attrs
                .get(ATTR_HREF)
                .with_reason(|| ErrorReason::PipelineLinkHrefNotFound {
                    rel: TYPE_COPY_DIR.into(),
                })?;
        let mut path = PathBuf::new();
        path.extend(href_attr.split('/'));
        if !path.is_absolute() {
            path = value.manifest_dir.join(path);
        }
        let target_path = value
            .attrs
            .get("data-target-path")
            .map(|m| Path::new(m).to_owned());

        Ok(Self {
            asset_input: value,
            path,
            target_path,
        })
    }
}

/// A trait that indicates a type can be used as config type for copy dir pipeline.
pub trait CopyDirConfig {
    /// Returns the directory where the output shoule write to.
    fn output_dir(&self) -> &Path;
}

/// A CopyDir asset pipeline.
#[derive(Debug)]
pub struct CopyDir<C> {
    /// Runtime build config.
    cfg: Arc<C>,
    /// Parsed inputs.
    inputs: Vec<Input>,
}

impl<C> CopyDir<C>
where
    C: CopyDirConfig,
{
    pub fn new(cfg: Arc<C>) -> Self {
        Self {
            cfg,
            inputs: Vec::new(),
        }
    }

    /// Run this pipeline.
    #[tracing::instrument(level = "trace", skip(cfg))]
    async fn run_with_input(cfg: &C, input: Input) -> Result<CopyDirOutput> {
        let rel_path = crate::util::strip_prefix(&input.path);
        tracing::info!(path = ?rel_path, "copying directory");

        let canonical_path =
            fs::canonicalize(&input.path)
                .await
                .with_reason(|| ErrorReason::FsNotExist {
                    path: input.path.to_owned(),
                })?;
        let dir_name = canonical_path
            .file_name()
            .with_reason(|| ErrorReason::PathNoFileStem {
                path: canonical_path.to_owned(),
            })?;

        let out_rel_path = input
            .target_path
            .as_deref()
            .unwrap_or_else(|| dir_name.as_ref());

        let dir_out = cfg.output_dir().join(out_rel_path);

        if !dir_out.starts_with(cfg.output_dir()) {
            return Err(ErrorReason::PipelineLinkDataTargetPathRelativeExpected {
                path: out_rel_path.to_owned(),
            }
            .into_error());
        }

        copy_dir_recursive(canonical_path, dir_out).await?;

        tracing::info!(path = ?rel_path, "finished copying directory");
        Ok(CopyDirOutput(input.asset_input.id))
    }
}

#[async_trait]
impl<C> Asset for CopyDir<C>
where
    C: 'static + CopyDirConfig + Send + Sync,
{
    type Output = CopyDirOutput;
    type OutputStream = BoxStream<'static, Result<Self::Output>>;

    async fn try_push_input(&mut self, input: AssetInput) -> Result<()> {
        let input = Input::try_from(input)?;

        self.inputs.push(input);

        Ok(())
    }

    async fn run_once(&self, input: super::AssetInput) -> Result<Self::Output> {
        let input = Input::try_from(input)?;

        Self::run_with_input(self.cfg.as_ref(), input).await
    }

    fn outputs(self) -> Self::OutputStream {
        let Self { cfg, inputs } = self;

        stream::iter(inputs)
            .then(move |input| {
                let cfg = cfg.clone();
                tokio::spawn(async move { Self::run_with_input(cfg.as_ref(), input).await })
            })
            .map(|m| match m.reason(ErrorReason::TokioTaskFailed) {
                Ok(Ok(m)) => Ok(m),
                Ok(Err(e)) | Err(e) => Err(e),
            })
            .boxed()
    }
}

/// The output of a CopyDir build pipeline.
pub struct CopyDirOutput(usize);

#[async_trait(?Send)]
impl Output for CopyDirOutput {
    async fn finalize(self, dom: &mut Document) -> Result<()> {
        dom.select(&trunk_id_selector(self.0)).remove();
        Ok(())
    }
}