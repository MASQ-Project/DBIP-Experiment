// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.
use crate::bootstrapper::PortConfiguration;
use crate::stream_messages::AddStreamMsg;
use crate::sub_lib::stream_connector::StreamConnector;
use crate::sub_lib::stream_connector::StreamConnectorReal;
use crate::sub_lib::tokio_wrappers::TokioListenerWrapper;
use crate::sub_lib::tokio_wrappers::TokioListenerWrapperReal;
use actix::Recipient;
use masq_lib::logger::Logger;
use std::future::Future;
use std::io;
use std::marker::Send;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

pub trait ListenerHandler: Send + Future {
    fn bind_port_and_configuration(
        &mut self,
        port: u16,
        port_configuration: PortConfiguration,
    ) -> io::Result<()>;
    fn bind_subs(&mut self, add_stream_sub: Recipient<AddStreamMsg>);
}

pub trait ListenerHandlerFactory: Send {
    fn make(&self) -> Box<dyn Future<Output = io::Result<()>>>;
}

pub struct ListenerHandlerReal {
    port: Option<u16>,
    port_configuration: Option<PortConfiguration>,
    listener: Box<dyn TokioListenerWrapper>,
    add_stream_sub: Option<Recipient<AddStreamMsg>>,
    stream_connector: Box<dyn StreamConnector>,
    logger: Logger,
}

impl ListenerHandler for ListenerHandlerReal {
    fn bind_port_and_configuration(
        &mut self,
        port: u16,
        port_configuration: PortConfiguration,
    ) -> io::Result<()> {
        self.port = Some(port);
        let is_clandestine = port_configuration.is_clandestine;
        self.port_configuration = Some(port_configuration);
        self.logger = Logger::new(&format!("ListenerHandler {}", port));
        let ip_addr = IpAddr::V4(if is_clandestine {
            Ipv4Addr::from(0)
        } else {
            Ipv4Addr::LOCALHOST
        });
        self.listener.bind(SocketAddr::new(ip_addr, port))
    }

    fn bind_subs(&mut self, add_stream_sub: Recipient<AddStreamMsg>) {
        self.add_stream_sub = Some(add_stream_sub);
    }
}

impl Future for ListenerHandlerReal {
    type Output = ();

    fn poll(self: Pin<&mut ListenerHandlerReal>, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            let result = self.listener.poll_accept(cx);
            match result {
                Poll::Ready(Ok((stream, socket_addr))) => {
                    let connection_info =
                        match self.stream_connector.split_stream(stream, &self.logger) {
                            Some(ci) => ci,
                            None => {
                                error!(
                                    self.logger,
                                    "Connection from {} was closed before it could be accepted",
                                    socket_addr
                                );
                                return Poll::Pending;
                            }
                        };
                    self.add_stream_sub
                        .as_ref()
                        .expect("Internal error: StreamHandlerPool unbound")
                        .try_send(AddStreamMsg::new(
                            connection_info,
                            self.port,
                            self.port_configuration
                                .as_ref()
                                .expect("Internal error: port_configuration is None")
                                .clone(),
                        ))
                        .expect("Internal error: StreamHandlerPool is dead");
                }
                Poll::Ready(Err(e)) => {
                    // TODO FIXME we should kill the entire Node if there is a fatal error in a listener_handler
                    // TODO this could be exploitable and inefficient: if we keep getting errors, we go into a tight loop and do not return
                    error!(self.logger, "Could not accept connection: {}", e);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl ListenerHandlerReal {
    fn new() -> ListenerHandlerReal {
        ListenerHandlerReal {
            port: None,
            port_configuration: None,
            listener: Box::new(TokioListenerWrapperReal::new()),
            add_stream_sub: None,
            stream_connector: Box::new(StreamConnectorReal {}),
            logger: Logger::new("Uninitialized Listener"),
        }
    }
}

pub struct ListenerHandlerFactoryReal {}

impl ListenerHandlerFactory for ListenerHandlerFactoryReal {
    fn make(&self) -> Box<dyn Future<Output = io::Result<()>>> {
        Box::new(ListenerHandlerReal::new())
    }
}

impl ListenerHandlerFactoryReal {
    pub fn new() -> ListenerHandlerFactoryReal {
        ListenerHandlerFactoryReal {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_test_utils::NullDiscriminatorFactory;
    use crate::test_utils::little_tcp_server::LittleTcpServer;
    use crate::test_utils::recorder::make_recorder;
    use crate::test_utils::recorder::Recorder;
    use crate::test_utils::stream_connector_mock::StreamConnectorMock;
    use crate::test_utils::unshared_test_utils::make_rt;
    use actix::Actor;
    use actix::Addr;
    use actix::System;
    use crossbeam_channel::unbounded;
    use masq_lib::test_utils::logging::init_test_logging;
    use masq_lib::test_utils::logging::TestLog;
    use masq_lib::test_utils::logging::TestLogHandler;
    use masq_lib::utils::{find_free_port, localhost};
    use std::cell::RefCell;
    use std::io::Error;
    use std::io::ErrorKind;
    use std::net;
    use std::net::Shutdown;
    use std::net::TcpStream as StdTcpStream;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;
    use tokio;
    use tokio::net::TcpStream;
    use tokio::task;

    struct TokioListenerWrapperMock {
        logger_arc: Arc<TestLog>,
        bind_results: Vec<io::Result<()>>,
        poll_accept_results: RefCell<Vec<Poll<io::Result<(TcpStream, SocketAddr)>>>>,
    }

    impl TokioListenerWrapperMock {
        fn new() -> TokioListenerWrapperMock {
            TokioListenerWrapperMock {
                logger_arc: Arc::new(TestLog::new()),
                bind_results: vec![],
                poll_accept_results: RefCell::new(vec![]),
            }
        }
    }

    impl TokioListenerWrapper for TokioListenerWrapperMock {
        fn bind(&mut self, addr: SocketAddr) -> io::Result<()> {
            self.logger_arc
                .lock()
                .unwrap()
                .log(format!("bind ({:?})", addr));
            self.bind_results.remove(0)
        }

        fn poll_accept(
            &mut self,
            cx: &mut Context<'_>,
        ) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
            self.poll_accept_results.borrow_mut().remove(0)
        }
    }

    impl TokioListenerWrapperMock {
        pub fn bind_result(mut self, result: io::Result<()>) -> TokioListenerWrapperMock {
            self.bind_results.push(result);
            self
        }

        pub fn poll_accept_results(
            self,
            result_vec: Vec<Poll<io::Result<(TcpStream, SocketAddr)>>>,
        ) -> TokioListenerWrapperMock {
            result_vec
                .into_iter()
                .for_each(|result| self.poll_accept_results.borrow_mut().push(result));
            self
        }
    }

    #[test]
    #[should_panic(expected = "TcpListener not initialized - bind to a SocketAddr")]
    fn panics_if_tried_to_run_without_initializing() {
        let subject = ListenerHandlerReal::new();
        make_rt().block_on(subject).unwrap();
    }

    #[test]
    fn handles_bind_port_and_configuration_failure() {
        let listener = TokioListenerWrapperMock::new()
            .bind_result(Err(Error::from(ErrorKind::AddrNotAvailable)));
        let discriminator_factory = NullDiscriminatorFactory::new();
        let mut subject = ListenerHandlerReal::new();
        subject.listener = Box::new(listener);

        let result = subject.bind_port_and_configuration(
            1234,
            PortConfiguration::new(vec![Box::new(discriminator_factory)], false),
        );

        assert_eq!(result.err().unwrap().kind(), ErrorKind::AddrNotAvailable);
    }

    #[test]
    fn handles_bind_port_and_configuration_success_for_clandestine_port() {
        let listener = TokioListenerWrapperMock::new().bind_result(Ok(()));
        let listener_log = listener.logger_arc.clone();
        let discriminator_factory =
            NullDiscriminatorFactory::new().discriminator_nature(vec![b"booga".to_vec()]);
        let mut subject = ListenerHandlerReal::new();
        subject.listener = Box::new(listener);

        let result = subject.bind_port_and_configuration(
            2345,
            PortConfiguration::new(vec![Box::new(discriminator_factory)], true),
        );

        assert_eq!(result.unwrap(), ());
        assert_eq!(listener_log.dump(), vec!(format!("bind (0.0.0.0:2345)")));
        assert_eq!(subject.port, Some(2345));
        let mut port_configuration = subject.port_configuration.unwrap();
        let factory = port_configuration.discriminator_factories.remove(0);
        let mut discriminator = factory.make();
        let chunk = discriminator.take_chunk().unwrap();
        assert_eq!(chunk.chunk, b"booga".to_vec());
        assert!(port_configuration.is_clandestine);
    }

    #[test]
    fn handles_bind_port_and_configuration_success_for_non_clandestine_port() {
        let listener = TokioListenerWrapperMock::new().bind_result(Ok(()));
        let listener_log = listener.logger_arc.clone();
        let discriminator_factory =
            NullDiscriminatorFactory::new().discriminator_nature(vec![b"booga".to_vec()]);
        let mut subject = ListenerHandlerReal::new();
        subject.listener = Box::new(listener);

        let result = subject.bind_port_and_configuration(
            2345,
            PortConfiguration::new(vec![Box::new(discriminator_factory)], false),
        );

        assert_eq!(result.unwrap(), ());
        assert_eq!(listener_log.dump(), vec!(format!("bind (127.0.0.1:2345)")));
        assert_eq!(subject.port, Some(2345));
        let mut port_configuration = subject.port_configuration.unwrap();
        let factory = port_configuration.discriminator_factories.remove(0);
        let mut discriminator = factory.make();
        let chunk = discriminator.take_chunk().unwrap();
        assert_eq!(chunk.chunk, b"booga".to_vec());
        assert!(!port_configuration.is_clandestine);
    }

    #[test]
    fn handles_connection_errors() {
        init_test_logging();
        let (stream_handler_pool, _, recording_arc) = make_recorder();

        let (tx, rx) = unbounded();
        thread::spawn(move || {
            let system = System::new();
            let add_stream_sub = start_recorder(stream_handler_pool);
            tx.send(add_stream_sub)
                .expect("Unable to send add_stream_sub to test");
            system.run();
        });

        let port = find_free_port();
        thread::spawn(move || {
            let add_stream_sub = rx.recv().unwrap();
            let tokio_listener_wrapper = TokioListenerWrapperMock::new()
                .bind_result(Ok(()))
                .poll_accept_results(vec![
                    Poll::Ready(Err(Error::from(ErrorKind::AddrInUse))),
                    Poll::Ready(Err(Error::from(ErrorKind::AddrNotAvailable))),
                    Poll::Pending,
                ]);
            let mut subject = ListenerHandlerReal::new();
            subject.listener = Box::new(tokio_listener_wrapper);
            subject.bind_subs(add_stream_sub);
            subject
                .bind_port_and_configuration(port, PortConfiguration::new(vec![], false))
                .unwrap();
            task::spawn(subject);
        });
        let tlh = TestLogHandler::new();
        tlh.await_log_containing("address not available", 1000);
        tlh.assert_logs_contain_in_order(vec![
            &format!(
                "ERROR: ListenerHandler {}: Could not accept connection: address in use",
                port
            )[..],
            &format!(
                "ERROR: ListenerHandler {}: Could not accept connection: address not available",
                port
            )[..],
        ]);
        let recording = recording_arc.lock().unwrap();
        assert_eq!(recording.len(), 0);
    }

    #[test]
    fn handles_connection_that_wont_split() {
        init_test_logging();
        let (stream_handler_pool, _, recording_arc) = make_recorder();

        let port = find_free_port();
        let server = LittleTcpServer::start();
        thread::spawn(move || {
            let add_stream_sub = start_recorder(stream_handler_pool);
            let std_stream = StdTcpStream::connect(server.socket_addr()).unwrap();
            let stream = TcpStream::from_std(std_stream).unwrap();
            let tokio_listener_wrapper = TokioListenerWrapperMock::new()
                .bind_result(Ok(()))
                .poll_accept_results(vec![Poll::Ready(Ok((
                    stream,
                    SocketAddr::from_str("1.2.3.4:5").unwrap(),
                )))]);
            let stream_connector = StreamConnectorMock::new().split_stream_result(None);
            let mut subject = ListenerHandlerReal::new();
            subject.listener = Box::new(tokio_listener_wrapper);
            subject.stream_connector = Box::new(stream_connector);
            subject.bind_subs(add_stream_sub);
            subject
                .bind_port_and_configuration(port, PortConfiguration::new(vec![], false))
                .unwrap();
            task::spawn(subject)
        });
        let tlh = TestLogHandler::new();
        // Stream has no peer_addr before splitting: {}
        tlh.await_log_containing(
            &format!(
                "ERROR: ListenerHandler {}: Connection from 1.2.3.4:5 was closed before it could be accepted",
                port,
            )[..],
            1000
        );
        let recording = recording_arc.lock().unwrap();
        assert_eq!(recording.len(), 0);
    }

    #[test]
    fn converts_connections_into_connection_infos() {
        let (stream_handler_pool, awaiter, recording_arc) = make_recorder();

        let (tx, rx) = unbounded();
        thread::spawn(move || {
            let system = System::new();
            let add_stream_sub = start_recorder(stream_handler_pool);
            tx.send(add_stream_sub).expect("Internal Error");
            system.run();
        });

        let port = find_free_port();
        thread::spawn(move || {
            let add_stream_sub = rx.recv().unwrap();
            let mut subject = ListenerHandlerReal::new();
            subject.bind_subs(add_stream_sub);
            subject
                .bind_port_and_configuration(port, PortConfiguration::new(vec![], false))
                .unwrap();
            task::spawn(subject)
        });

        // todo fixme wait for listener to be running in a better way
        thread::sleep(Duration::from_millis(500));

        let socket_addr = SocketAddr::new(localhost(), port);
        let x = net::TcpStream::connect(socket_addr).unwrap();
        let y = net::TcpStream::connect(socket_addr).unwrap();
        let z = net::TcpStream::connect(socket_addr).unwrap();
        let (x_addr, y_addr, z_addr) = (
            x.local_addr().unwrap(),
            y.local_addr().unwrap(),
            z.local_addr().unwrap(),
        );
        x.shutdown(Shutdown::Both).unwrap();
        y.shutdown(Shutdown::Both).unwrap();
        z.shutdown(Shutdown::Both).unwrap();

        awaiter.await_message_count(3);
        let recording = recording_arc.lock().unwrap();
        assert_eq!(
            recording
                .get_record::<AddStreamMsg>(0)
                .connection_info
                .peer_addr,
            x_addr
        );
        assert_eq!(
            recording
                .get_record::<AddStreamMsg>(1)
                .connection_info
                .peer_addr,
            y_addr
        );
        assert_eq!(
            recording
                .get_record::<AddStreamMsg>(2)
                .connection_info
                .peer_addr,
            z_addr
        );
        assert_eq!(recording.len(), 3);
    }

    fn start_recorder(recorder: Recorder) -> Recipient<AddStreamMsg> {
        let recorder_addr: Addr<Recorder> = recorder.start();
        recorder_addr.recipient::<AddStreamMsg>()
    }
}
