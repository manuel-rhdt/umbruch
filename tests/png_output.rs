#![cfg(feature = "png-output")]

mod common;

use std::io::Cursor;

use assert_cmd::prelude::*;
use predicates::prelude::*;

#[test]
fn paints_the_selected_page_at_the_requested_resolution() -> Result<(), Box<dyn std::error::Error>>
{
    let input = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("png-pages.html");
    let image = input.with_extension("png");
    let mut encoder = png::Encoder::new(std::fs::File::create(image)?, 1, 1);
    encoder.set_color(png::ColorType::Rgb);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&[0, 255, 0])?;
    writer.finish()?;
    std::fs::write(
        &input,
        r#"<!doctype html>
        <style>
          @page { size: 120px 96px; margin: 0 }
          body, p { margin: 0 }
          .patch { width: 20px; height: 20px; background: red }
          .second { break-before: page }
          .second .patch { background: blue; opacity: .5; transform: translateX(10px) }
          p { font: 18px "DejaVu Sans" }
          img { display: block; margin-left: 80px; width: 16px; height: 16px }
        </style>
        <section><div class="patch"></div></section>
        <section class="second"><div class="patch"></div><p>Shaped text</p><img src="png-pages.png"></section>"#,
    )?;
    let result = common::umbruch()?
        .args(["--png-page", "2", "--png-dpi", "192"])
        .arg(&input)
        .arg("-")
        .assert()
        .success();
    let mut reader = png::Decoder::new(Cursor::new(&result.get_output().stdout)).read_info()?;
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut pixels)?;
    assert_eq!((frame.width, frame.height), (240, 192));
    assert_eq!(frame.color_type, png::ColorType::Rgba);
    assert_eq!(reader.info().pixel_dims.unwrap().xppu, 7559);
    let pixel = |x: usize, y: usize| &pixels[(y * 240 + x) * 4..][..4];
    assert_eq!(pixel(5, 5)[3], 0);
    let patch = pixel(30, 10);
    assert_eq!(&patch[..3], &[0, 0, 255]);
    assert!((127..=128).contains(&patch[3]));
    assert!(
        pixels[40 * 240 * 4..]
            .chunks_exact(4)
            .any(|pixel| pixel == [0, 0, 0, 255])
    );
    assert!(
        pixels
            .chunks_exact(4)
            .any(|pixel| pixel == [0, 255, 0, 255])
    );

    common::umbruch()?
        .args(["--png-page", "3"])
        .arg(&input)
        .arg("-")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("out of range"));
    Ok(())
}

#[test]
fn paints_inline_and_external_svg_paths_through_their_clip()
-> Result<(), Box<dyn std::error::Error>> {
    let input = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("png-svg-paths.html");
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="60" height="40" viewBox="0 0 30 20">
        <defs><clipPath id="cut"><path d="M4 3 H20 V17 H4 Z"/></clipPath></defs>
        <g transform="translate(2 1)" clip-path="url(#cut)">
          <path d="M0 0 H30 V20 H0 Z" fill="#f00"/>
          <path d="M6 10 H16" fill="none" stroke="#00f" stroke-width="2" stroke-linecap="round"/>
        </g>
        </svg>"##;
    std::fs::write(input.with_extension("svg"), svg)?;
    std::fs::write(
        &input,
        format!(
            r#"<!doctype html>
        <style>@page {{ size: 96px 136px; margin: 0 }}
        body {{ margin: 0 }} svg, img {{ display: block }}</style>
        {svg}<img src="png-svg-paths.svg" alt="Path sample">
        <img src="png-svg-paths.svg" alt="Repeated path sample">"#
        ),
    )?;
    let result = common::umbruch()?
        .args(["--png-page", "1"])
        .arg(&input)
        .arg("-")
        .assert()
        .success();
    let mut reader = png::Decoder::new(Cursor::new(&result.get_output().stdout)).read_info()?;
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut pixels)?;
    assert_eq!((frame.width, frame.height), (96, 136));
    assert_eq!(frame.color_type, png::ColorType::Rgba);
    let pixel = |x: usize, y: usize| &pixels[(y * 96 + x) * 4..][..4];
    for y in [0, 40, 80] {
        assert_eq!(pixel(16, y + 12), &[255, 0, 0, 255]);
        assert_eq!(pixel(25, y + 22), &[0, 0, 255, 255]);
        assert_eq!(pixel(5, y + 20)[3], 0);
        assert_eq!(pixel(50, y + 22)[3], 0);
    }
    use umbruch::display_list::DisplayItem;
    use umbruch::engine::{Document, DocumentKind, Engine, EngineConfig, LayoutOptions};
    let engine = Engine::new(EngineConfig {
        system_fonts: false,
        ..EngineConfig::default()
    })?;
    let mut document = Document::new(std::fs::read_to_string(&input)?, DocumentKind::Html);
    document.base_dir = input.parent().map(std::path::Path::to_path_buf);
    let mut pages = engine
        .layout(&document, &LayoutOptions::default())?
        .into_print_pages();
    let scenes: Vec<_> = pages.pages[0]
        .display_list
        .items()
        .filter_map(|item| match item {
            DisplayItem::DrawSubscene { scene, .. } => Some(scene),
            _ => None,
        })
        .collect();
    assert_eq!(scenes.len(), 3);
    assert!(std::sync::Arc::ptr_eq(scenes[1], scenes[2]));
    Ok(())
}

#[test]
fn paints_css_gradient_backgrounds() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("png-css-gradients.html");
    std::fs::write(
        &input,
        r#"<!doctype html>
        <style>
          @page { size: 80px 160px; margin: 0 }
          body { margin: 0 }
          div { width: 80px; height: 40px }
          .linear { background: linear-gradient(to right, transparent, blue) }
          .radial { background: radial-gradient(40px 16px, red 50%, blue 50%);
                    transform: translateX(.5px) }
          .conic { background: conic-gradient(from 90deg, red 25%, blue 25% 50%, lime 50%) }
          .repeating { background: repeating-linear-gradient(to right, red 0 10px, blue 10px 20px) }
        </style>
        <div class="linear"></div><div class="radial"></div>
        <div class="conic"></div><div class="repeating"></div>"#,
    )?;
    let result = common::umbruch()?
        .args(["--png-page", "1"])
        .arg(input)
        .arg("-")
        .assert()
        .success();
    let mut reader = png::Decoder::new(Cursor::new(&result.get_output().stdout)).read_info()?;
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut pixels)?;
    assert_eq!((frame.width, frame.height), (80, 160));
    let pixel = |x: usize, y: usize| &pixels[(y * 80 + x) * 4..][..4];
    let red = [255, 0, 0, 255];
    let blue = [0, 0, 255, 255];

    let middle = pixel(40, 20);
    assert_eq!(&middle[..3], &[0, 0, 255]);
    assert!((125..=131).contains(&middle[3]));
    assert_eq!(pixel(55, 60), &red);
    assert_eq!(pixel(40, 70), &blue);
    let edge = pixel(0, 60);
    assert_eq!(&edge[..3], &[0, 0, 255]);
    assert!((127..=128).contains(&edge[3]));
    assert_eq!(pixel(55, 109), &red);
    assert_eq!(pixel(25, 109), &blue);
    assert_eq!(pixel(55, 91), &[0, 255, 0, 255]);
    for x in [3, 23, 43, 63] {
        assert_eq!(pixel(x, 140), &red);
        assert_eq!(pixel(x + 10, 140), &blue);
    }
    Ok(())
}

#[test]
fn paints_svg_gradients_in_a_transformed_and_clipped_subscene()
-> Result<(), Box<dyn std::error::Error>> {
    let input = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("png-svg-gradients.html");
    std::fs::write(
        &input,
        r##"<!doctype html>
        <style>
          @page { size: 120px 70px; margin: 0 }
          body { margin: 0 }
          svg { display: block }
        </style>
        <svg width="120" height="70" viewBox="0 0 60 35">
          <defs>
            <linearGradient id="linear" gradientUnits="userSpaceOnUse" x1="0" x2="20"
                            gradientTransform="translate(24) rotate(90)">
              <stop stop-color="red"/>
              <stop offset="1" stop-color="blue"/>
            </linearGradient>
            <radialGradient id="radial">
              <stop stop-color="red" stop-opacity=".5"/>
              <stop offset="1" stop-color="blue" stop-opacity=".5"/>
            </radialGradient>
          </defs>
          <g transform="translate(3 4)">
            <path d="M0 0 H24 V20 H0 Z M8 6 H16 V14 H8 Z" fill-rule="evenodd" fill="url(#linear)"/>
            <rect x="30" width="20" height="10" fill="url(#radial)"/>
          </g>
        </svg>"##,
    )?;
    let result = common::umbruch()?
        .env("RUST_LOG", "warn")
        .args(["--png-page", "1"])
        .arg(input)
        .arg("-")
        .assert()
        .success()
        .stderr(predicate::str::contains("unsupported drawing features").not());
    let mut reader = png::Decoder::new(Cursor::new(&result.get_output().stdout)).read_info()?;
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut pixels)?;
    assert_eq!((frame.width, frame.height), (120, 70));
    let pixel = |x: usize, y: usize| &pixels[(y * 120 + x) * 4..][..4];
    assert!(pixel(12, 11)[0] > 225);
    assert!(pixel(12, 44)[2] > 225);
    assert_eq!(pixel(28, 28)[3], 0);
    assert_eq!(pixel(4, 28)[3], 0);
    let center = pixel(85, 17);
    assert!(center[0] > 230 && (127..=128).contains(&center[3]));
    assert!(pixel(68, 17)[2] > 215);
    Ok(())
}
