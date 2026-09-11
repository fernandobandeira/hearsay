//! Typed errors. Nothing in narrator panics on a bad book, a missing model or a
//! wedged subprocess: every failure path here has a caller that logs it and
//! degrades — a chunk that will not render is a chunk the renderer moves past,
//! not a dead process.

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TtsError {
    #[error("espeak-ng: {0}")]
    Espeak(String),
    #[error("kokoro model: {0}")]
    Model(String),
    #[error("voice {0}: {1}")]
    Voice(String, String),
    #[error("inference: {0}")]
    Infer(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum BookError {
    #[error("not an epub: {0}")]
    NotEpub(String),
    #[error("epub: {0}")]
    Epub(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum PackError {
    #[error("chapter {0} is {1} chunk(s) short of complete")]
    Incomplete(usize, usize),
    #[error("chapter {0} has no chunks")]
    Empty(usize),
    #[error("not a wav: {0}")]
    NotWav(String),
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}
