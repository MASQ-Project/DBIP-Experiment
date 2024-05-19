// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.
use std::io;
use tokio::io::AsyncWrite;

pub struct StdStreams<'a> {
    pub stdin: &'a mut (dyn io::Read + Send),
    pub stdout: &'a mut (dyn io::Write + Send),
    pub stderr: &'a mut (dyn io::Write + Send),
}

pub trait Command<T> {
    fn go(&mut self, streams: &mut StdStreams<'_>, args: &[String]) -> T;
}

pub struct AsyncStdStreams<'main_fn>{
    pub stdin: &'main_fn mut (dyn AsyncWrite + Send + Unpin),
    pub stdout: &'main_fn mut (dyn AsyncWrite + Send + Unpin),
    pub stderr: &'main_fn mut  (dyn io::Write + Send + Unpin),
}
