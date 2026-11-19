//! Windows offline track using the Visual Enhancer portable release as a separate process.
use super::{Image, JobCtx, Ordering, Outcome, Rendered, Tick, Value, Work, json, lock};
use filmcraft_export::{ColorSignal, ExportError, ProResMovFormat, ProResMovFrame, Progress};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(super) struct Settings {
    cli: PathBuf,
    style: u64,
    strength: f64,
}

fn controls(params: &Value) -> Result<(u64, f64), String> {
    let style = params.get("style").map_or(Some(0), Value::as_u64).filter(|s| *s <= 2).ok_or("style must be 0, 1 or 2")?;
    let strength = params.get("strength").map_or(Some(1.0), Value::as_f64).filter(|v| (0.0..=2.0).contains(v)).ok_or("strength must be between 0 and 2")?;
    Ok((style, strength))
}

impl Settings {
    pub(super) fn from_params(params: &Value) -> Result<Self, String> {
        let (style, strength) = controls(params)?;
        let cli = std::env::var_os("FILMCRAFT_VE_CLI")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("FilmCraft-SDKs/VisualEnhancer/v23.6/VE_CLI.exe")))
            .ok_or("set FILMCRAFT_VE_CLI to the absolute path of VE_CLI.exe")?;
        for tool in [&cli, &cli.with_file_name("ffmpeg.exe"), &cli.with_file_name("ffprobe.exe")] {
            if !tool.is_absolute() || !tool.is_file() {
                return Err(format!(
                    "Missing Visual Enhancer tool at {}. Set FILMCRAFT_VE_CLI to the full path of VE_CLI.exe in the unpacked portable release.",
                    tool.display()
                ));
            }
        }
        Ok(Self { cli, style, strength })
    }
}

fn check_cancel(prog: &Progress) -> io::Result<()> {
    if prog.cancel.load(Ordering::Relaxed) { Err(io::Error::other("track rendering cancelled")) } else { Ok(()) }
}

/// Files avoid pipe backpressure during long renders. Limit diagnostics brought back into the UI.
fn run(cmd: &mut Command, dir: &Path, prog: &Progress) -> io::Result<String> {
    check_cancel(prog)?;
    let mut output = tempfile::tempfile_in(dir)?;
    let mut errors = tempfile::tempfile_in(dir)?;
    let mut child = cmd.creation_flags(0x0800_0000).stdin(Stdio::null()).stdout(output.try_clone()?).stderr(errors.try_clone()?).spawn()?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if !prog.cancel.load(Ordering::Relaxed) => std::thread::sleep(std::time::Duration::from_millis(100)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                result?;
                return Err(io::Error::other("track rendering cancelled"));
            }
        }
    };
    let tail = |f: &mut File| -> io::Result<String> {
        f.seek(io::SeekFrom::Start(f.metadata()?.len().saturating_sub(8192)))?;
        let mut bytes = Vec::new();
        f.take(8192).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    if !status.success() {
        return Err(io::Error::other(format!("{} exited with {status}: {} {}", cmd.get_program().to_string_lossy(), tail(&mut errors)?, tail(&mut output)?)));
    }
    tail(&mut output)
}

/// Windows can briefly retain a file handle after a cancelled process exits.
fn clean_scratch(scratch: tempfile::TempDir) -> io::Result<()> {
    let path = scratch.path().to_owned();
    let mut error = match scratch.close() {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        match fs::remove_dir_all(&path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => error = e,
        }
    }
    Err(io::Error::new(error.kind(), format!("could not clean {}: {error}", path.display())))
}

fn matches_rate(value: &Value, rate: filmcraft_time::FrameRate) -> bool {
    let Some((n, d)) = value.as_str().and_then(|v| v.split_once('/')) else {
        return false;
    };
    let (Ok(n), Ok(d)) = (n.parse::<u32>(), d.parse::<u32>()) else {
        return false;
    };
    n > 0 && d > 0 && i128::from(n) * i128::from(rate.den) == i128::from(d) * i128::from(rate.num)
}

impl JobCtx {
    fn stage_external_source(&self, w: &Work, dir: &Path, prog: &Progress) -> Result<bool, String> {
        let pipe = self.project.sequence(self.seq).ok_or("sequence is missing")?.settings.color;
        let hdr = pipe.working.is_hdr();
        // VE's HDR encoder produces PQ. Encode the working light to PQ for both HDR working spaces.
        let target = if hdr { filmcraft_color::ColorSpace::Rec2100Pq } else { filmcraft_color::ColorSpace::Rec709 };
        let transform = filmcraft_color::OutputTransform::new(&pipe, target);
        let format = ProResMovFormat {
            profile: filmcraft_export::prores_profile("4444"),
            signal: if hdr { ColorSignal::PQ } else { ColorSignal::default() },
            alpha: true,
        };
        let fd = self.rate.frame_duration();
        let frames = (0..w.frames).map(|fi| {
            check_cancel(prog).map_err(|_| ExportError::Cancelled)?;
            let img = self.render(w.track_index, w.start + Tick(fi.saturating_mul(fd.0)));
            if (img.w, img.h) != self.size || img.w.checked_mul(img.h).and_then(|v| v.checked_mul(4)) != Some(img.px.len()) {
                return Err(ExportError::Encode("unexpected source frame size".into()));
            }
            let mut rgb = Vec::with_capacity(img.px.len() / 4 * 3);
            let mut alpha = Vec::with_capacity(img.px.len() / 4);
            for px in img.px.as_chunks::<4>().0 {
                if !px.iter().all(|v| v.is_finite()) {
                    return Err(ExportError::Encode("non-finite source pixel".into()));
                }
                rgb.extend(transform.encode(Image::unpremul(*px)));
                alpha.push((px[3].clamp(0.0, 1.0) * 1023.0).round() as u16);
            }
            *lock(&prog.status) = format!("Preparing Neural Rendering source: frame {} of {}", fi + 1, w.frames);
            Ok(ProResMovFrame { rgb, alpha: Some(alpha) })
        });
        let source = File::create(dir.join("source.mov")).map_err(|e| e.to_string())?;
        let (writer, _) = filmcraft_export::encode_prores_mov_frames(BufWriter::new(source), self.size.0 as u32, self.size.1 as u32, self.rate, format, frames)
            .map_err(|e| e.to_string())?;
        writer.into_inner().map_err(|e| e.to_string())?;
        Ok(hdr)
    }

    pub(super) fn render_external(&self, w: &Work, prog: &Progress, out: &mut Outcome) -> Result<Rendered, String> {
        let dest = std::path::absolute(&w.path).map_err(|e| e.to_string())?;
        let parent = dest.parent().ok_or("invalid track path")?;
        let scratch = tempfile::Builder::new().prefix(".filmcraft-nr-").tempdir_in(parent).map_err(|e| e.to_string())?;
        let track = tempfile::Builder::new().prefix(".filmcraft-track-").suffix(".mov").tempfile_in(parent).map_err(|e| e.to_string())?.into_temp_path();
        let render = self.write_external(w, prog, scratch.path(), &track);
        clean_scratch(scratch).map_err(|e| e.to_string())?;
        render?;
        check_cancel(prog).map_err(|e| e.to_string())?;
        track.persist_noclobber(&dest).map_err(|e| e.to_string())?;
        let frames = w.frames as u64;
        out.reconstruction_hw += frames;
        out.reconstruction_hw_name = Some("Visual Enhancer / NeuroFrame Neural Rendering");
        prog.done.fetch_add(frames, Ordering::Relaxed);
        Ok(Rendered { clip: w.clip, start: w.start, dur: w.dur, path: w.path.clone(), bytes: None })
    }

    fn write_external(&self, w: &Work, prog: &Progress, dir: &Path, track: &Path) -> Result<(), String> {
        let settings = self.external.as_ref().ok_or("missing Visual Enhancer settings")?;
        let hdr = self.stage_external_source(w, dir, prog)?;
        let preset = json!({"format":"dlss5-visual-enhancer-settings-preset","schema_version":41,"name":"FilmCraft",
            "settings":{"video_enabled_stages":["neural_model"],"hdr_mode":hdr,"codec":"FFV1 Lossless RGB 10-bit","container":"MKV"}});
        fs::write(dir.join("settings.json"), preset.to_string()).map_err(|e| e.to_string())?;
        *lock(&prog.status) = "Running Visual Enhancer Neural Rendering".into();
        run(
            Command::new(&settings.cli)
                .arg("render")
                .arg(dir.join("source.mov"))
                .arg("--preset")
                .arg(dir.join("settings.json"))
                .args(["--style", &settings.style.to_string(), "--strength", &settings.strength.to_string(), "--codec", "ffv1", "--container", "mkv", "-o"])
                .arg(dir.join("enhanced.mkv"))
                .args(["--json", "--quiet"]),
            dir,
            prog,
        )
        .map_err(|e| e.to_string())?;
        let probe = |file: &Path| -> Result<Value, String> {
            let text = run(
                Command::new(settings.cli.with_file_name("ffprobe.exe"))
                    .args([
                        "-v",
                        "error",
                        "-select_streams",
                        "v:0",
                        "-count_packets",
                        "-show_entries",
                        "stream=width,height,nb_read_packets,pix_fmt,color_transfer,color_primaries,r_frame_rate",
                        "-of",
                        "json",
                    ])
                    .arg(file),
                dir,
                prog,
            )
            .map_err(|e| e.to_string())?;
            let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            let stream = &v["streams"][0];
            if stream["nb_read_packets"].as_str().and_then(|s| s.parse::<i64>().ok()) != Some(w.frames)
                || stream["width"].as_u64() != Some(self.size.0 as u64)
                || stream["height"].as_u64() != Some(self.size.1 as u64)
            {
                return Err(format!("Unexpected frame count or dimensions in {}: {stream}", file.display()));
            }
            Ok(stream.clone())
        };
        let stream = probe(&dir.join("enhanced.mkv"))?;
        if stream["pix_fmt"] != "gbrp10le" || (hdr && (stream["color_transfer"] != "smpte2084" || stream["color_primaries"] != "bt2020")) {
            return Err(format!("Unexpected Visual Enhancer output signal: {stream}"));
        }
        *lock(&prog.status) = "Restoring source alpha and writing the track".into();
        let (matrix, space, primaries, transfer) = if hdr { ("bt2020", "bt2020nc", "bt2020", "smpte2084") } else { ("bt709", "bt709", "bt709", "bt709") };
        let timebase = format!("settb={}/{},setpts=N", self.rate.den, self.rate.num);
        // Give both inputs the same four-plane format. Converting alpha to a grayscale video
        // plane would remap its full range into limited-range luma before the merge.
        let filters = format!(
            "[0:v]{timebase},scale=out_color_matrix={matrix}:out_range=tv,format=yuva444p10le,setsar=1[rgb];[1:v]{timebase},format=yuva444p10le,setsar=1[a];[rgb][a]mergeplanes=0x00010213:yuva444p10le,setparams=range=limited:color_primaries={primaries}:color_trc={transfer}:colorspace={space}[v]"
        );
        let rate = format!("{}/{}", self.rate.num, self.rate.den);
        run(
            Command::new(settings.cli.with_file_name("ffmpeg.exe"))
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
                .arg(dir.join("enhanced.mkv"))
                .arg("-i")
                .arg(dir.join("source.mov"))
                .args([
                    "-filter_complex",
                    &filters,
                    "-map",
                    "[v]",
                    "-an",
                    "-c:v",
                    "prores_ks",
                    "-profile:v",
                    "4",
                    "-alpha_bits",
                    "16",
                    "-color_range",
                    "tv",
                    "-colorspace",
                    space,
                    "-color_primaries",
                    primaries,
                    "-color_trc",
                    transfer,
                    "-r",
                    &rate,
                    "-movflags",
                    "+write_colr",
                    "-fps_mode",
                    "cfr",
                    "-video_track_timescale",
                    &self.rate.num.to_string(),
                ])
                .arg(track),
            dir,
            prog,
        )
        .map_err(|e| e.to_string())?;
        let track = probe(track)?;
        if !track["pix_fmt"].as_str().is_some_and(|s| s.starts_with("yuva444p"))
            || track["color_transfer"] != transfer
            || track["color_primaries"] != primaries
            || !matches_rate(&track["r_frame_rate"], self.rate)
        {
            return Err(format!("Unexpected ProRes track format: {track}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_waits_for_a_transient_windows_file_lock() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let file = File::options().write(true).create_new(true).share_mode(0).open(path.join("source.mov")).unwrap();
        assert!(fs::remove_dir_all(&path).is_err());
        std::thread::scope(|s| {
            s.spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(250));
                drop(file);
            });
            clean_scratch(dir).unwrap();
        });
        assert!(!path.exists());
    }

    #[test]
    fn validates_neural_controls_before_starting_a_job() {
        assert_eq!(controls(&json!({})).unwrap(), (0, 1.0));
        assert_eq!(controls(&json!({"style":2,"strength":0.5})).unwrap(), (2, 0.5));
        for bad in [json!({"style":3}), json!({"style":-1}), json!({"strength":2.1}), json!({"strength":"1"})] {
            assert!(controls(&bad).is_err());
        }
    }
}
