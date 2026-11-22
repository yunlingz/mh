//! Explicit HTTP/HTTPS environment proxies with redacted startup observations.
//!
//! Read only the six named variables. Do not inherit ALL_PROXY or OS settings.
//! Preserve the HTTP client's CGI safeguard without changing process variables.

use std::ffi::OsString;

const VARIABLES: [&str; 6] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueStatus {
    Set,
    Empty,
    NonUnicode,
}

impl ValueStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Empty => "empty",
            Self::NonUnicode => "non-Unicode; ignored",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    pub name: &'static str,
    pub status: ValueStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Environment {
        variables: Vec<Variable>,
        http: Option<&'static str>,
        https: Option<&'static str>,
        cgi: bool,
    },
    CustomClient,
}

impl Status {
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::CustomClient => {
                vec!["Custom HTTP client; proxy behavior is client-defined".into()]
            }
            Self::Environment {
                variables,
                http,
                https,
                cgi,
            } => {
                let mut lines = vec!["Automatic HTTP/HTTPS environment proxies".into()];
                lines.push(http.map_or_else(
                    || "HTTP: direct".into(),
                    |name| format!("HTTP: proxy configured from {name}"),
                ));
                lines.push(https.map_or_else(
                    || "HTTPS: direct".into(),
                    |name| format!("HTTPS: proxy configured from {name}"),
                ));
                lines.extend(
                    variables
                        .iter()
                        .map(|variable| format!("{}: {}", variable.name, variable.status.label())),
                );
                if *cgi {
                    lines.push("REQUEST_METHOD detected; CGI protection disables proxies".into());
                }
                lines
            }
        }
    }
}

pub fn configure(
    builder: reqwest::ClientBuilder,
) -> Result<(reqwest::ClientBuilder, Status), String> {
    configure_with(builder, |name| std::env::var_os(name))
}

pub fn configure_with(
    builder: reqwest::ClientBuilder,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<(reqwest::ClientBuilder, Status), String> {
    let values = VARIABLES.map(&lookup);
    let cgi = lookup("REQUEST_METHOD").is_some();
    let variables = VARIABLES
        .iter()
        .zip(&values)
        .filter_map(|(name, value)| {
            value.as_ref().map(|value| Variable {
                name,
                status: if value.is_empty() {
                    ValueStatus::Empty
                } else if value.to_str().is_some() {
                    ValueStatus::Set
                } else {
                    ValueStatus::NonUnicode
                },
            })
        })
        .collect();
    let mut builder = builder.no_proxy();
    let mut http = None;
    let mut https = None;
    if !cgi {
        let exclusions = selected(&values, 4).and_then(|(_, value)| {
            // hyper-util 0.1.21 does not apply domain wildcards to IP hosts.
            let rules = if value.split(',').any(|entry| entry.trim() == "*") {
                "*,0.0.0.0/0,::/0"
            } else {
                value
            };
            reqwest::NoProxy::from_string(rules)
        });
        for (index, scheme) in [(0, "HTTP"), (2, "HTTPS")] {
            if let Some((name, value)) = selected(&values, index)
                && !value.is_empty()
            {
                let proxy = if index == 0 {
                    reqwest::Proxy::http(value)
                } else {
                    reqwest::Proxy::https(value)
                }
                .map_err(|_| format!("Invalid {scheme} proxy setting in {name}"))?;
                builder = builder.proxy(proxy.no_proxy(exclusions.clone()));
                if index == 0 {
                    http = Some(name);
                } else {
                    https = Some(name);
                }
            }
        }
    }
    Ok((
        builder,
        Status::Environment {
            variables,
            http,
            https,
            cgi,
        },
    ))
}

fn selected(values: &[Option<OsString>; 6], index: usize) -> Option<(&'static str, &str)> {
    [index, index + 1].into_iter().find_map(|index| {
        values[index]
            .as_ref()
            .and_then(|value| value.to_str())
            .map(|value| (VARIABLES[index], value))
    })
}
