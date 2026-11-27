//! Shaders from the title itself.
//!
//! A container copied into `<files>/victory/shaders` is a DXIL program the host
//! can translate and draw with, which is the closest thing to the title's own
//! graphics this port can reach before x86-64 execution exists: the machine
//! code is the title's, the translation is the title's, and the only thing the
//! host supplies is a full-screen vertex stage and a 2D attachment.
//!
//! The rule for taking one is deliberately narrow, and it is what this title's
//! image offers: every pixel shader in it takes `SV_Position`, one `float4`
//! varying and writes one to four `float4` colours, binding no resources. The
//! one that writes several is an engine clear that fills a group of targets in
//! one pass, and the host draws it into that many attachments. Anything else is
//! reported and skipped rather than guessed at.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use victory_dxil::{Stage, Translation};

/// A pixel shader of the title's that this host can run.
pub struct Pixel {
    /// The container's file name, for the log line.
    pub name: String,
    pub translation: Translation,
    /// How many `float4` colours it writes, at locations 0 and up: one for the
    /// clears and copies, several for the multi-target one.
    pub colours: u32,
}

/// Every container in `dir` a pipeline can be built around.
///
/// Nothing there is not a failure: the host falls back to the shader it ships
/// with.
pub fn find(dir: Option<&Path>) -> Result<Vec<Pixel>> {
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };
    let mut containers: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "dxil")
            })
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    containers.sort();

    let mut found = Vec::new();
    for path in containers {
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let bytes = fs::read(&path).with_context(|| format!("reading {name}"))?;
        let translation = match victory_dxil::translate(&bytes) {
            Ok(translation) => translation,
            Err(error) => {
                tracing::warn!(shader = %name, %error, "the title's container does not translate");
                continue;
            }
        };
        match interface(&translation) {
            Ok(Interface { varying, colours }) => {
                tracing::info!(
                    shader = %name,
                    stage = ?translation.stage,
                    varying = varying > 0,
                    colours,
                    words = translation.words.len(),
                    "the title's shader runs"
                );
                found.push(Pixel {
                    name,
                    translation,
                    colours,
                });
            }
            Err(error) => {
                tracing::info!(shader = %name, reason = format!("{error}"), "skipping the title's shader");
            }
        }
    }
    Ok(found)
}

/// What the host's vertex stage has to hand the pixel stage.
pub struct Interface {
    /// Whether the shader reads the `float4` varying at location 1.
    pub varying: u32,
    /// How many `float4` colours it writes, at locations 0 and up.
    pub colours: u32,
}

/// Reads the pixel stage's interface out of the translated module.
///
/// Only the shape the host can feed is accepted: one to four `float4` colours
/// at locations 0 and up, each of them written, and either nothing or one
/// `float4` varying at location 1.
fn interface(translation: &Translation) -> Result<Interface> {
    if translation.stage != Stage::Pixel {
        anyhow::bail!("it is a {:?} shader, not a pixel shader", translation.stage);
    }

    let words = &translation.words;
    let mut floats: Vec<u32> = Vec::new();
    let mut vectors: Vec<(u32, u32, u32)> = Vec::new();
    let mut pointers: Vec<(u32, u32)> = Vec::new();
    let mut variables: Vec<(u32, u32, u32)> = Vec::new();
    let mut locations: Vec<(u32, u32)> = Vec::new();
    let mut builtins = 0usize;
    let mut bound = 0usize;

    let mut at = 5;
    while at < words.len() {
        let opcode = words[at] & 0xFFFF;
        let length = (words[at] >> 16) as usize;
        if length == 0 || at + length > words.len() {
            break;
        }
        match opcode {
            22 => floats.push(words[at + 1]),
            // `OpTypeVector %result %component type %count`.
            23 => vectors.push((words[at + 1], words[at + 3], words[at + 2])),
            32 => pointers.push((words[at + 1], words[at + 3])),
            59 => variables.push((words[at + 2], words[at + 3], words[at + 1])),
            71 => match words[at + 2] {
                11 => builtins += 1,
                30 => locations.push((words[at + 1], words[at + 3])),
                33 => bound += 1,
                _ => {}
            },
            _ => {}
        }
        at += length;
    }

    if bound > 0 {
        anyhow::bail!("it binds resources, which this host does not feed yet");
    }

    // A `float4` type is a vector of four floats; anything else is out of scope.
    let float4 = |id: u32| {
        vectors
            .iter()
            .find(|(result, _, _)| *result == id)
            .is_some_and(|(_, components, scalar)| *components == 4 && floats.contains(scalar))
    };
    let pointee = |variable: u32| {
        variables
            .iter()
            .find(|(result, _, _)| *result == variable)
            .and_then(|(_, _, pointer)| pointers.iter().find(|(result, _)| result == pointer))
            .map(|(_, pointee)| *pointee)
    };

    let mut written: Vec<u32> = Vec::new();
    let mut varying = 0u32;
    for (variable, storage, _) in &variables {
        let Some(location) = locations
            .iter()
            .find(|(target, _)| target == variable)
            .map(|(_, location)| *location)
        else {
            continue;
        };
        let Some(pointee) = pointee(*variable) else {
            continue;
        };
        match (storage, location) {
            (3, _) if float4(pointee) => written.push(location),
            (3, _) => anyhow::bail!("its colour output at location {location} is not a float4"),
            (1, 1) if float4(pointee) => varying = 1,
            (1, 1) => anyhow::bail!("its varying at location 1 is not a float4"),
            (1, _) => anyhow::bail!("it reads a varying at location {location}"),
            _ => {}
        }
    }
    written.sort_unstable();
    // A Vulkan pass has to declare every attachment between zero and the last
    // one the shader writes, so a gap is a shape this host cannot build.
    if written.first() != Some(&0) || written.iter().enumerate().any(|(at, location)| *location as usize != at)
    {
        anyhow::bail!("it writes colour locations {written:?}, which skip one");
    }
    let colours = written.len() as u32;
    let _ = builtins;
    Ok(Interface { varying, colours })
}
