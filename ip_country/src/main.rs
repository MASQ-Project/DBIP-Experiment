use std::io;
use std::env;
use std::process;
use ip_country_lib::ip_country::ip_country;

pub fn main() {
    process::exit(ip_country(env::args().collect(), &mut io::stdin(), &mut io::stdout(), &mut io::stderr()))
}
