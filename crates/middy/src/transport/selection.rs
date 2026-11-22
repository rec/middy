//! Shared name and shell-pattern selection for MIDI discovery.

use super::Port;
use std::io;

pub(crate) fn select_port<'a>(ports: &'a [Port], pattern: Option<&str>) -> io::Result<&'a Port> {
    Ok(&ports[select_port_index(ports.iter().map(|p| p.name()), pattern)?])
}
/// Shared selection policy for native ports and Python transport descriptors.
#[doc(hidden)]
pub fn select_port_index<'a>(
    names: impl IntoIterator<Item = &'a str>,
    pattern: Option<&str>,
) -> io::Result<usize> {
    let mut names = names.into_iter().enumerate();
    let Some(pattern) = pattern else {
        return names
            .next()
            .map(|(i, _)| i)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no MIDI ports available"));
    };
    if pattern.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "port pattern must not be empty",
        ));
    }
    let mut matches = names.filter(|(_, n)| port_matches(pattern, n));
    let (index, _) = matches
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no matching MIDI port"))?;
    if matches.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "port pattern matches multiple ports",
        ));
    }
    Ok(index)
}
pub(crate) fn port_matches(pattern: &str, name: &str) -> bool {
    name.contains(pattern) || wildcard(pattern, name)
}
fn wildcard(pattern: &str, name: &str) -> bool {
    let p: Vec<_> = pattern.chars().collect();
    let n: Vec<_> = name.chars().collect();
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < n.len() {
        if i < p.len() && p[i] == '*' {
            star = Some(i);
            i += 1;
            retry = j;
        } else if let Some(next) = pattern_character(&p, i, n[j]) {
            i = next;
            j += 1;
        } else if let Some(s) = star {
            retry += 1;
            j = retry;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}
fn pattern_character(pattern: &[char], start: usize, value: char) -> Option<usize> {
    let c = *pattern.get(start)?;
    if c == '?' {
        return Some(start + 1);
    }
    if c != '[' {
        return (c == value).then_some(start + 1);
    }
    let mut first = start + 1;
    let negate = pattern.get(first) == Some(&'!');
    first += usize::from(negate);
    let mut end = first + usize::from(pattern.get(first) == Some(&']'));
    while end < pattern.len() && pattern[end] != ']' {
        end += 1;
    }
    if end == pattern.len() {
        return (value == '[').then_some(start + 1);
    }
    let mut matched = false;
    let mut i = first;
    while i < end {
        if i + 2 < end && pattern[i + 1] == '-' {
            matched |= pattern[i] <= value && value <= pattern[i + 2];
            i += 3;
        } else {
            matched |= pattern[i] == value;
            i += 1;
        }
    }
    (matched != negate).then_some(end + 1)
}
