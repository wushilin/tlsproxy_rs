use std::{fmt::Display, str::FromStr, sync::Arc};

use lazy_static::lazy_static;
use regex::Regex;

pub struct HostAndPort {
    host: String,
    port: u16,
}

lazy_static! {
    static ref SUPPORTED_REGEX_LIST:Arc<Vec<Regex>> = Arc::new(vec![
        Regex::new(r"(?i)^\s*host\s*=\s*(\S+)\s*,\s*port\s*=\s*(\d+)\s*$").unwrap(),
        Regex::new(r"(?i)^\s*(\S+)\s*:\s*(\d+)\s*$").unwrap(),
        Regex::new(r"(?i)^\s*(\S+)\s*@\s*(\d+)\s*$").unwrap(),
        Regex::new(r"(?i)^\s*(\S+)\s*|\s*(\d+)\s*$").unwrap(),
    ]);
}

impl HostAndPort {
    pub fn new(host:String, port: u16) -> Self {
        return Self {
            host,
            port,
        }
    }

    // Parse a host port spec. If host is sufficient, return the parsed result
    // If it is failing, return host:default_port
    pub fn parse_or_default(host:&str, default_port: u16) -> Self {
        let result = host.parse();
        match result {
            Err(_cause) => {
                return HostAndPort{
                    host: host.into(),
                    port: default_port,
                }
            },
            Ok(inner_result) => {
                return inner_result;
            }
        }
    }
    pub fn to_string(&self) -> String {
        return format!("{}:{}", self.host, self.port)
    }
}

pub struct ParseError {
    cause: String
}

impl Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.cause)
    }
}
impl FromStr for HostAndPort {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        for reg in SUPPORTED_REGEX_LIST.iter() {
            let next = try_match(reg, input);
            match next {
                Some(result) => {
                    return result;
                },
                None => {
                    continue;
                }
            }
        }
        return Err(ParseError{cause: format!("unsupported HostAndPort spec `{input}`. Support `host:port`, `host@port`, `host|port`, `host=xxx,port=yyy` formats only")})
    }
}

fn try_match(regex:&Regex, input:&str) -> Option<Result<HostAndPort, ParseError>> {
    if let Some(caps) = regex.captures(input) {
        let host:String = caps[1].into();
        let port_str:String = caps[2].into();
        let port_result = port_str.parse();
        match port_result {
            Ok(port) => {
                return Some(Ok(HostAndPort {
                    host,
                    port,
                }));
            },
            Err(_cause) => {
                return Some(Err(ParseError{cause: format!("{port_str} (in {input}) is not valid port")}));
            }
        }
    }
    return None;
}