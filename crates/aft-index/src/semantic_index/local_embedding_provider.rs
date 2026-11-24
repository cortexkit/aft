pub trait LocalEmbeddingProvider: Send {
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}
