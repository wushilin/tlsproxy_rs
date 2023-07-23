use std::{collections::HashMap, io::Read};
use serde::{Deserialize, Serialize};

use std::error::Error;


#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct HostAndPort {
    pub host:String,
    pub port:i32
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResolveEntry {
    pub from:HostAndPort,
    pub to:HostAndPort
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResolveConfigRaw {
    rules: Vec<ResolveEntry>
}

#[derive(Debug, Clone)]
pub struct ResolveConfig {
    rules: HashMap<HostAndPort, HostAndPort>
}
impl ResolveConfig {
    fn load_raw_from_json(path:&str)-> Result<ResolveConfigRaw, Box<dyn Error>> {
        let mut f = std::fs::File::open(path)?;
        let mut str = String::new();
        f.read_to_string(&mut str)?;
        let resolve_config_raw = serde_json::from_str(&str)?;
        return Ok(resolve_config_raw);
    }

    pub fn empty() -> Result<ResolveConfig, Box<dyn Error>> {
        let rules = HashMap::<HostAndPort, HostAndPort>::new();
        return Ok(ResolveConfig { rules })
    }
    
    pub fn resolve(&self, host:&str, port: i32) -> Option<HostAndPort> {
        let lookup = HostAndPort{host:String::from(host), port: port};
        let lookup_result = self.rules.get(&lookup);
        return match lookup_result {
            Some(what)=> {
                Some(what.clone())
            },
            None => {
                None
            }
        }
    }
    
    pub fn load_from_json_file(path:&str) -> Result<ResolveConfig, Box<dyn Error>> {
        let raw = Self::load_raw_from_json(path)?;
        let mut rules = HashMap::<HostAndPort, HostAndPort>::new();
        for i in raw.rules {
            let key = i.from.clone();
            let value = i.to.clone();
            rules.insert(key, value);
        }

        return Ok(ResolveConfig{rules});
    }
}