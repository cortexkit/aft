use super::*;

#[derive(Debug, Clone)]
pub enum ArtifactSource {
    Registered {
        paths: TaskPaths,
        artifact: TaskArtifact,
    },
    #[cfg(test)]
    Exact(PathBuf),
}

#[derive(Debug, Clone)]
pub enum BgBuffer {
    Pipes {
        stdout: ArtifactSource,
        stderr: ArtifactSource,
    },
    Pty {
        combined: ArtifactSource,
    },
}
