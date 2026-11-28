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
