use std::{collections::HashMap, io::Read};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::error::Error;

use super::errors::GeneralError;

/// Represents a HostName and Port pair. The hostname is required where port is optional
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct HostAndPort {
    /// Host name
    pub host:String,
    /// Port number
    pub port:Option<i32>
}

impl HostAndPort {
    /// Parse www.google.com:443, or www.google.com to HostAndPort. The former one has Port -> Some(443) while later has Port -> None
    pub fn parse_single(input:&str) -> Result<HostAndPort, Box<dyn Error>> {
        let first_colon = input.find(":");
        match first_colon {
            None => {
                return Ok(HostAndPort{host:String::from(input), port: None})
            },
            Some(index) => {
                let first = &input[..index];
                let second = &input[index + 1..];
                let port:i32 = second.parse()?;
                return Ok(HostAndPort{host:String::from(first), port: Some(port)})
            }
        }
    }

    /// Parse multiple HostAndPort separated by ;. e.g. www.google.com:443;www.baidu.com;www.yahoo.com.sg:443. If any parse failed, the whole parse is abondoned.
    pub fn parse_as_vec(input: &str) -> Result<Vec<HostAndPort>, Box<dyn Error>> {
        let vec = input.split(";").collect::<Vec<&str>>();
        let mut result_vec = Vec::<HostAndPort>::new();
        for next in vec {
            let trimmed = next.trim();
            if trimmed.len() == 0 {
                continue;
            }
            let next_host = Self::parse_single(trimmed)?;
            result_vec.push(next_host);
        }
        return Ok(result_vec);
    }
}

/// Represents a resolve config. It internally stores Keys as HostAndPort and values as HostAndPort as well.
#[derive(Debug, Clone)]
pub struct ResolveConfig {
    rules: HashMap<HostAndPort, HostAndPort>
}


/// Default ResolveConfig. The mapping is empty.
impl Default for ResolveConfig {
    fn default() -> ResolveConfig {
        ResolveConfig {
            rules: HashMap::new()
        }
    }
}

impl ResolveConfig {
    fn load_value_from_json(path:&str)-> Result<Value, Box<dyn Error>> {
        let mut f = std::fs::File::open(path)?;
        let mut str = String::new();
        f.read_to_string(&mut str)?;
        let resolve_config_raw = serde_json::from_str(&str)?;
        return Ok(resolve_config_raw);
    }
    
    /// Resolve a lookup host and port to a target HostAndPort. 
    /// If there is entry with given host and port, then exact lookup is returned.
    /// If there is entry with given host, then the lookup without port is returned.
    /// 
    /// If looked up result has no port,the input port is used as default.
    pub fn resolve(&self, host:&str, port: i32) -> Option<(String, i32)> {
        if self.rules.len() == 0 {
            return Some((String::from(host), port));
        }
        let mut lookup = HostAndPort{host:String::from(host), port: Some(port)};
        let mut lookup_result = self.rules.get(&lookup);
        if lookup_result.is_none() {
            // Specific port lookup is not found. Checking none port specific
            lookup.port = None;
            lookup_result = self.rules.get(&lookup);
        }
        return match lookup_result {
            Some(what)=> {
                let result_clone = what.clone();
                let result_host = result_clone.host;
                let result_port = result_clone.port.unwrap_or(port);
                Some((result_host, result_port))
            },
            None => {
                None
            }
        }
    }
    
    fn value_to_string(value:Value) -> Result<String, Box<dyn Error>> {
        match value {
            Value::String(what) => {
                return Ok(what);
            },
            _ => {
                return Err(GeneralError::wrap_box(
                    format!("Unexpected JSON type `{value}`. Expect String")));
            }
        }
    }

    /// Parse ResolveConfig from a Json. The JSON must be a String -> String structure. If key or value is not string, the parse fails.
    pub fn load_from_json_file(path:&str) -> Result<ResolveConfig, Box<dyn Error>> {
        let raw = Self::load_value_from_json(path)?;
        match raw {
            Value::Object(map) => {
                let mut rules = HashMap::<HostAndPort, HostAndPort>::new();
                for (k, vraw) in map {
                    let v = Self::value_to_string(vraw)?;
                    let keys = HostAndPort::parse_as_vec(&k)?;
                    let value = HostAndPort::parse_single(&v)?;
                    for next_key in keys {
                        rules.insert(next_key.clone(), value.clone());
                    }
                }
                return Ok(ResolveConfig{rules});
            },
            _ => {
                return Err(GeneralError::wrap_box(format!("Unexpected JSON type `{raw}`. Expect Map")));
            }
        }
    }
}