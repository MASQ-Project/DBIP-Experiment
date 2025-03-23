// Copyright (c) 2024, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::bit_queue::BitQueue;
use crate::country_block_serde::{CountryBlockSerializer, FinalBitQueue};
use crate::country_block_stream::CountryBlock;
use crate::ip_country_csv::CSVParser;
use crate::ip_country_mmdb::MMDBParser;
use std::sync::{Arc, Mutex, MutexGuard};
use std::cell::RefCell;
use std::io;
use std::any::Any;

const COUNTRY_BLOCK_BIT_SIZE: usize = 64;

#[allow(unused_must_use)]
pub fn ip_country(
    args: Vec<String>,
    stdin: &mut dyn io::Read,
    stdout: &mut dyn io::Write,
    stderr: &mut dyn io::Write,
    parser_factory: &dyn DBIPParserFactory,
) -> i32 {
    let parser = parser_factory.make(&args);
    let mut errors: Vec<String> = vec![];
    let (final_ipv4, final_ipv6, countries_opt) = parser.parse(stdin, &mut errors);
    if let Err(error) = generate_rust_code(final_ipv4, final_ipv6, countries_opt, stdout) {
        errors.push(format!("Error generating Rust code: {:?}", error))
    }
    if errors.is_empty() {
        0
    } else {
        let error_list = errors.join("\n");
        write!(
            stdout,
            r#"
            *** DO NOT USE THIS CODE ***
            It will produce incorrect results.
            The process that generated it found these errors:

{}

            Fix the errors and regenerate the code.
            *** DO NOT USE THIS CODE ***
"#,
            error_list
        );
        write!(stderr, "{}", error_list);
        1
    }
}

pub trait DBIPParserFactory {
    fn make(&self, args: &Vec<String>) -> Box<dyn DBIPParser>;
}

pub struct DBIPParserFactoryReal {}

impl DBIPParserFactory for DBIPParserFactoryReal {
    fn make(&self, args: &Vec<String>) -> Box<dyn DBIPParser> {
        if args.contains(&"--csv".to_string()) {
            return Box::new(CSVParser {})
        }
        return Box::new(MMDBParser {})
    }
}

pub trait DBIPParser: Any {
    fn as_any(&self) -> &dyn Any;

    fn parse(
        &self,
        stdin: &mut dyn io::Read,
        errors: &mut Vec<String>,
    ) -> (FinalBitQueue, FinalBitQueue, Option<Vec<(String, String)>>);
}

fn generate_rust_code(
    final_ipv4: FinalBitQueue,
    final_ipv6: FinalBitQueue,
    countries_opt: Option<Vec<(String, String)>>,
    output: &mut dyn io::Write,
) -> Result<(), io::Error> {
    write!(output, "\n// GENERATED CODE: REGENERATE, DO NOT MODIFY!\n")?;
    generate_country_block_code(
        "ipv4_country",
        final_ipv4.bit_queue,
        output,
        final_ipv4.block_count,
    )?;
    generate_country_block_code(
        "ipv6_country",
        final_ipv6.bit_queue,
        output,
        final_ipv6.block_count,
    )?;
    Ok(())
}

fn generate_country_block_code(
    name: &str,
    mut bit_queue: BitQueue,
    output: &mut dyn io::Write,
    block_count: usize,
) -> Result<(), io::Error> {
    let bit_queue_len = bit_queue.len();
    writeln!(output)?;
    writeln!(output, "pub fn {}_data() -> (Vec<u64>, usize) {{", name)?;
    writeln!(output, "    (")?;
    write!(output, "        vec![")?;
    let mut values_written = 0usize;
    while bit_queue.len() >= COUNTRY_BLOCK_BIT_SIZE {
        write_value(
            &mut bit_queue,
            COUNTRY_BLOCK_BIT_SIZE,
            &mut values_written,
            output,
        )?;
    }
    if !bit_queue.is_empty() {
        let bit_count = bit_queue.len();
        write_value(&mut bit_queue, bit_count, &mut values_written, output)?;
    }
    write!(output, "\n        ],\n")?;
    writeln!(output, "        {}", bit_queue_len)?;
    writeln!(output, "    )")?;
    writeln!(output, "}}")?;
    writeln!(output, "\npub fn {}_block_count() -> usize {{", name)?;
    writeln!(output, "        {}", block_count)?;
    writeln!(output, "}}")?;
    Ok(())
}

fn write_value(
    bit_queue: &mut BitQueue,
    bit_count: usize,
    values_written: &mut usize,
    output: &mut dyn io::Write,
) -> Result<(), io::Error> {
    if (*values_written & 0b11) == 0 {
        write!(output, "\n            ")?;
    } else {
        write!(output, " ")?;
    }
    let value = bit_queue
        .take_bits(bit_count)
        .expect("There should be bits left!");
    write!(output, "0x{:016X},", value)?;
    *values_written += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};
    use test_utilities::byte_array_reader_writer::{ByteArrayReader, ByteArrayWriter};
    use std::any::TypeId;

    struct DBIPParserMock{
        parse_params: Arc<Mutex<Vec<Vec<String>>>>,
        parse_errors: RefCell<Vec<Vec<String>>>,
        parse_results: RefCell<Vec<(FinalBitQueue, FinalBitQueue, Option<Vec<(String, String)>>)>>,
    }

    impl DBIPParser for DBIPParserMock {
        fn as_any(&self) -> &dyn Any {
            return self;
        }

        fn parse(
            &self,
            stdin: &mut dyn io::Read,
            errors: &mut Vec<String>,
        ) -> (FinalBitQueue, FinalBitQueue, Option<Vec<(String, String)>>) {
            self.parse_params.lock().unwrap().push(errors.clone());
            errors.extend(self.parse_errors.borrow_mut().remove(0));
            self.parse_results.borrow_mut().remove(0)
        }
    }

    impl DBIPParserMock {
        pub fn new() -> Self {
            Self {
                parse_params: Arc::new(Mutex::new(vec![])),
                parse_errors: RefCell::new(vec![]),
                parse_results: RefCell::new(vec![]),
            }
        }

        pub fn parse_params(
            mut self,
            params: &Arc<Mutex<Vec<Vec<String>>>>,
        ) -> Self {
            self.parse_params = params.clone();
            self
        }

        pub fn parse_errors(
            mut self,
            errors: Vec<&str>
        ) -> Self {
            self.parse_errors.borrow_mut().push(errors.into_iter().map(|s| s.to_string()).collect());
            self
        }

        pub fn parse_result(
            mut self,
            result: (FinalBitQueue, FinalBitQueue, Option<Vec<(String, String)>>)
        ) -> Self {
            self.parse_results.borrow_mut().push(result);
            self
        }
    }

    struct DBIPParserFactoryMock{
        make_params: Arc<Mutex<Vec<Vec<String>>>>,
        make_results: RefCell<Vec<DBIPParserMock>>,
    }

    impl DBIPParserFactory for DBIPParserFactoryMock {
        fn make(&self, args: &Vec<String>) -> Box<dyn DBIPParser> {
            self.make_params.lock().unwrap().push(args.clone());
            return Box::new(self.make_results.borrow_mut().remove(0))
        }
    }

    impl DBIPParserFactoryMock {
        pub fn new() -> Self {
            Self {
                make_params: Arc::new(Mutex::new(vec![])),
                make_results: RefCell::new(vec![]),
            }
        }

        fn make_params(
            mut self,
            params: &Arc<Mutex<Vec<Vec<String>>>>,
        ) -> Self {
            self.make_params = params.clone();
            self
        }

        fn make_result(mut self, result: DBIPParserMock) -> Self {
            self.make_results.borrow_mut().push(result);
            self
        }
    }

    static TEST_DATA: &str = "I represent test data arriving on standard input.";

    #[test]
    fn csv_makes_csv() {
        let subject = DBIPParserFactoryReal{};

        let result = subject.make(&vec!["--csv".to_string()]);

        assert_eq!((*result).as_any().type_id(), TypeId::of::<CSVParser>());
    }

    #[test]
    fn mmdb_makes_mmdb() {
        let subject = DBIPParserFactoryReal{};

        let result = subject.make(&vec!["--mmdb".to_string()]);

        assert_eq!((*result).as_any().type_id(), TypeId::of::<MMDBParser>());
    }

    #[test]
    fn nothing_makes_mmdb() {
        let subject = DBIPParserFactoryReal{};

        let result = subject.make(&vec![]);

        assert_eq!((*result).as_any().type_id(), TypeId::of::<MMDBParser>());
    }

    #[test]
    fn happy_path_csv_test() {
        let mut stdin = ByteArrayReader::new(TEST_DATA.as_bytes());
        let mut stdout = ByteArrayWriter::new();
        let mut stderr = ByteArrayWriter::new();
        let parse_params_arc = Arc::new(Mutex::new(vec![]));
        let ipv4_result = final_bit_queue(0x1122334455667788, 12);
        let ipv6_result = final_bit_queue(0x8877665544332211, 21);
        let parser = DBIPParserMock::new()
            .parse_params(&parse_params_arc)
            .parse_errors(vec![])
            .parse_result((ipv4_result, ipv6_result, None));
        let make_params_arc = Arc::new(Mutex::new(vec![]));
        let parser_factory = DBIPParserFactoryMock::new()
            .make_params(&make_params_arc)
            .make_result(parser);
        let args = vec!["--csv".to_string()];

        let result = ip_country(args.clone(), &mut stdin, &mut stdout, &mut stderr, &parser_factory);

        assert_eq!(result, 0);
        let make_params = make_params_arc.lock().unwrap();
        assert_eq!(*make_params, vec![args.clone()]);
        let parse_params = parse_params_arc.lock().unwrap();
        let expected_parse_params: Vec<Vec<String>> = vec![vec![]];
        assert_eq!(*parse_params, expected_parse_params);
        let stdout_string = String::from_utf8(stdout.get_bytes()).unwrap();
        let stderr_string = String::from_utf8(stderr.get_bytes()).unwrap();
        assert_eq!(
            stdout_string,
            r#"
// GENERATED CODE: REGENERATE, DO NOT MODIFY!

pub fn ipv4_country_data() -> (Vec<u64>, usize) {
    (
        vec![
            0x1122334455667788,
        ],
        64
    )
}

pub fn ipv4_country_block_count() -> usize {
        12
}

pub fn ipv6_country_data() -> (Vec<u64>, usize) {
    (
        vec![
            0x8877665544332211,
        ],
        64
    )
}

pub fn ipv6_country_block_count() -> usize {
        21
}
"#
                .to_string()
        );
        assert_eq!(stderr_string, "".to_string());
    }

    #[test]
    fn sad_path_test() {
        let mut stdin = ByteArrayReader::new(TEST_DATA.as_bytes());
        let mut stdout = ByteArrayWriter::new();
        let mut stderr = ByteArrayWriter::new();
        let parse_params_arc = Arc::new(Mutex::new(vec![]));
        let ipv4_result = final_bit_queue(0x1122334455667788, 12);
        let ipv6_result = final_bit_queue(0x8877665544332211, 21);
        let parser = DBIPParserMock::new()
            .parse_params(&parse_params_arc)
            .parse_errors(vec![
                "First error",
                "Second error"
            ])
            .parse_result((ipv4_result, ipv6_result, None));
        let make_params_arc = Arc::new(Mutex::new(vec![]));
        let parser_factory = DBIPParserFactoryMock::new()
            .make_params(&make_params_arc)
            .make_result(parser);
        let args = vec!["--csv".to_string()];

        let result = ip_country(args.clone(), &mut stdin, &mut stdout, &mut stderr, &parser_factory);

        assert_eq!(result, 1);
        let make_params = make_params_arc.lock().unwrap();
        assert_eq!(*make_params, vec![args.clone()]);
        let parse_params = parse_params_arc.lock().unwrap();
        let expected_parse_params: Vec<Vec<String>> = vec![vec![]];
        assert_eq!(*parse_params, expected_parse_params);
        let stdout_string = String::from_utf8(stdout.get_bytes()).unwrap();
        let stderr_string = String::from_utf8(stderr.get_bytes()).unwrap();
        assert_eq!(
            stdout_string,
            r#"
// GENERATED CODE: REGENERATE, DO NOT MODIFY!

pub fn ipv4_country_data() -> (Vec<u64>, usize) {
    (
        vec![
            0x1122334455667788,
        ],
        64
    )
}

pub fn ipv4_country_block_count() -> usize {
        12
}

pub fn ipv6_country_data() -> (Vec<u64>, usize) {
    (
        vec![
            0x8877665544332211,
        ],
        64
    )
}

pub fn ipv6_country_block_count() -> usize {
        21
}

            *** DO NOT USE THIS CODE ***
            It will produce incorrect results.
            The process that generated it found these errors:

First error
Second error

            Fix the errors and regenerate the code.
            *** DO NOT USE THIS CODE ***
"#
                .to_string()
        );
        assert_eq!(stderr_string,
r#"First error
Second error"#
                       .to_string()
        );
    }

    #[test]
    fn write_error_from_ip_country() {
        let stdin = &mut ByteArrayReader::new(TEST_DATA.as_bytes());
        let stdout = &mut ByteArrayWriter::new();
        let stderr = &mut ByteArrayWriter::new();
        stdout.reject_next_write(Error::new(ErrorKind::WriteZero, "Bad file Descriptor"));
        let factory = DBIPParserFactoryReal{};

        let result = ip_country(vec!["--csv".to_string()], stdin, stdout, stderr, &factory);

        assert_eq!(result, 1);
        let stdout_string = String::from_utf8(stdout.get_bytes()).unwrap();
        let stderr_string = String::from_utf8(stderr.get_bytes()).unwrap();
        assert_eq!(stderr_string, "Error generating Rust code: Custom { kind: WriteZero, error: \"Bad file Descriptor\" }");
        assert_eq!(stdout_string, "\n            *** DO NOT USE THIS CODE ***\n            It will produce incorrect results.\n            The process that generated it found these errors:\n\nError generating Rust code: Custom { kind: WriteZero, error: \"Bad file Descriptor\" }\n\n            Fix the errors and regenerate the code.\n            *** DO NOT USE THIS CODE ***\n");
    }

    fn final_bit_queue(contents: u64, block_count: usize) -> FinalBitQueue {
        let mut bit_queue = BitQueue::new();
        bit_queue.add_bits(contents, 64);
        FinalBitQueue {bit_queue, block_count}
    }
}
