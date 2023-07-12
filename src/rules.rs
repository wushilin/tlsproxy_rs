use regex::Regex;
use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::Read;
use serde_json::Value;
#[derive(Debug)]
pub struct RuleSet {
    default_allow: bool,
    allowed_all: bool,
    rejected_all: bool,
    allowed_static_hosts: HashMap<String, bool>,
    rejected_static_hosts: HashMap<String, bool>,
    allowed_patterns: Vec<Regex>,
    rejected_patterns: Vec<Regex>,
}

#[derive(Debug)]
pub struct RuleSetRaw {
    no_match_decision: String,
    whitelist: Vec<String>,
    blacklist: Vec<String>,
}

impl RuleSetRaw {
    fn from_json_value(val:&serde_json::Value) -> RuleSetRaw {
        let no_match_decision = Self::value_to_string(val.get("no_match_decision").unwrap());
        let whitelist = Self::value_to_vec(val.get("whitelist").unwrap());
        let blacklist = Self::value_to_vec(val.get("blacklist").unwrap());
        let result = RuleSetRaw { 
            no_match_decision, 
            whitelist, 
            blacklist,
        };

        return result;
    }

    fn value_to_string(val:&Value) -> String {
        match val {
            Value::String(some) => {
                return some.clone();
            },
            _ => {
                return String::from("");
            }
        }
    }
    fn value_to_vec(what:&serde_json::Value) -> Vec<String>{
        match what {
            Value::Array(what) => {
                return what.iter().map(|x| -> String {
                    match x {
                        Value::String(str) => {
                            str.clone()
                        },
                        _ => {
                            String::from("")
                        }
                    }
                }).filter( |x| -> bool {x.len() > 0}).collect();
            },
            _ => {
                return vec![];
            }
        }
    }

    fn generate(&self) -> Result<RuleSet, Box<dyn Error>> {
        let mut result = RuleSet {
            default_allow: false,
            allowed_static_hosts: HashMap::new(),
            allowed_patterns: Vec::new(),
            rejected_static_hosts: HashMap::new(),
            rejected_patterns: Vec::new(),
            allowed_all: false,
            rejected_all: false,
        };

        match self.no_match_decision.to_lowercase().as_str() {
            "accept" | "allow" => result.default_allow = true,
            "reject" | "deny" => result.default_allow = false,
            "" => return Err("required field `no_match_decision` not found".into()),
            _ => return Err(format!("unknown decision [{}], expect allow|reject", self.no_match_decision).into()),
        }

        for rule in &self.whitelist {
            let rule = rule.to_lowercase();
            if rule == "$any" {
                result.allowed_all = true;
                continue;
            }
            if rule.starts_with("host:") {
                result.allowed_static_hosts.insert(rule[5..].to_owned(), true);
            } else if rule.starts_with("pattern:") {
                let mut pattern = rule[8..].to_owned();
                if !pattern.starts_with("(?i)") {
                    pattern = "(?i)".to_owned() + &pattern;
                }
                result.allowed_patterns.push(Regex::new(&pattern)?);
            } else {
                return Err(format!("Unknown rule [{}], expect to begin with `host:` or `pattern:`", rule).into());
            }
        }

        for rule in &self.blacklist {
            let rule = rule.to_lowercase();
            if rule == "$any" {
                result.rejected_all = true;
                continue;
            }
            if rule.starts_with("host:") {
                result.rejected_static_hosts.insert(rule[5..].to_owned(), true);
            } else if rule.starts_with("pattern:") {
                let mut pattern = rule[8..].to_owned();
                if !pattern.starts_with("(?i)") {
                    pattern = "(?i)".to_owned() + &pattern;
                }
                result.rejected_patterns.push(Regex::new(&pattern)?);
            } else {
                return Err(format!("Unknown rule [{}], expect to begin with `host:` or `pattern:`", rule).into());
            }
        }

        Ok(result)
    }
}

pub fn parse(file: &str) -> Result<RuleSet, Box<dyn Error>> {
    let mut buffer = String::new();
    File::open(file)?.read_to_string(&mut buffer)?;

    let result_value: serde_json::Value = serde_json::from_str(&buffer)?;

    let result = RuleSetRaw::from_json_value(&result_value);

    result.generate()
}

impl RuleSet {
    pub fn check_access(&self, target_host: &str) -> bool {
        if self.default_allow {
            if self.rejected_all {
                return false;
            }
            !check_match(target_host, &self.rejected_static_hosts, &self.rejected_patterns)
        } else {
            if self.allowed_all {
                return true;
            }
            check_match(target_host, &self.allowed_static_hosts, &self.allowed_patterns)
        }
    }
}

fn check_match(host: &str, map: &HashMap<String, bool>, patterns: &[Regex]) -> bool {
    let host = host.to_lowercase();
    if map.contains_key(&host) {
        return true;
    }
    for pattern in patterns {
        if pattern.is_match(&host) {
            return true;
        }
    }
    false
}
