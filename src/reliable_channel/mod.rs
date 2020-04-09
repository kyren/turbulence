mod channel;
mod windows;

#[cfg(test)]
mod tests;

pub use channel::{Error, ReliableChannel, Settings};
