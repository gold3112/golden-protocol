use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::sync::OnceLock;

static MODEL: OnceLock<TextEmbedding> = OnceLock::new();

/// ローカルembeddingモデルを初期化 (初回のみダウンロード)
pub fn init_model() -> Result<()> {
    let model = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_show_download_progress(true),
    )?;
    MODEL.set(model).ok();
    Ok(())
}

/// テキスト → embeddingベクトル
pub fn embed(text: &str) -> Result<Vec<f32>> {
    let model = MODEL.get().ok_or_else(|| anyhow::anyhow!("model not initialized"))?;
    let mut results = model.embed(vec![text], None)?;
    Ok(results.remove(0))
}

/// 複数テキストを一括embed (効率的)
pub fn embed_batch(texts: Vec<&str>) -> Result<Vec<Vec<f32>>> {
    let model = MODEL.get().ok_or_else(|| anyhow::anyhow!("model not initialized"))?;
    Ok(model.embed(texts, None)?)
}
