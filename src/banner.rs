//! 全絵文字を並べたバナー画像。旧版の `CreateMontage` の移植。
//!
//! 縦横比 3:1 に近い並べ方を決め、各絵文字を正方形のセルにアスペクト比を保って収め、セルの 5% の余白を空けて並べ、
//! 幅 1100px の PNG にする。絵文字の数だけの原寸の画像を同時には持たないよう、1 枚ずつ読んでタイルの大きさに縮める。

use std::io::Cursor;
use std::path::PathBuf;

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat, Rgba, RgbaImage, imageops};
use tracing::warn;

/// バナーの幅。
pub const WIDTH: u32 = 1100;
/// 目指す縦横比（幅, 高さ）。Misskey（2026.3.2）はバナーを cover で、幅と高さの比が 2.9〜3.9 の枠に表示する
/// （ユーザーページ 734×250、ポップアップ 300×78）。2:1 では上下が大きく切れる。
const RATIO: (usize, usize) = (3, 1);
/// セルの幅に対する余白の割合。
const MARGIN: f64 = 0.05;

/// `count` 個を並べる（列、行）。最後の行に空き枠が残る並べ方も試し、縦横比が `ratio`（幅, 高さ）にいちばん近いものを選ぶ。
/// 近さは比で測り（2 倍の横長と 2 倍の縦長は同じ）、同じなら空き枠の少ない方、それも同じなら行の少ない方。
/// 行か列が丸ごと空く並べ方は、余白の帯ができるだけなので、比率が近くても試さない。
pub fn tile_geometry(count: usize, ratio: (usize, usize)) -> Option<(u32, u32)> {
    let (width, height) = ratio;
    // ずれ（大きい方 / 小さい方）を分子と分母で返す。浮動小数で割らずに掛け合わせて比べ、同じずれを同じと判定する。
    let spread = |(cols, rows): (usize, usize)| {
        let (x, y) = (cols * height, rows * width);
        (x.max(y), x.min(y))
    };
    (1..=count)
        .map(|rows| (count.div_ceil(rows), rows))
        // 列数はその行数に収まる最小なので、丸ごと空く列は無い。その列数に収まる最小の行数と同じなら、丸ごと空く行も無い。
        .filter(|&(cols, rows)| count.div_ceil(cols) == rows)
        .min_by(|&a, &b| {
            let ((a_hi, a_lo), (b_hi, b_lo)) = (spread(a), spread(b));
            (a_hi * b_lo)
                .cmp(&(b_hi * a_lo))
                .then((a.0 * a.1).cmp(&(b.0 * b.1)))
        })
        .map(|(cols, rows)| (cols as u32, rows as u32))
}

/// `images` を並べたバナー。空なら `None`。
pub fn montage(images: &[DynamicImage]) -> Option<RgbaImage> {
    let (cols, rows) = tile_geometry(images.len(), RATIO)?;
    let tile = WIDTH / cols;
    let margin = (f64::from(tile) * MARGIN) as u32;
    let cell = tile + margin * 2;
    let mut canvas = RgbaImage::from_pixel(cols * cell, rows * cell, Rgba([255, 255, 255, 255]));
    for (index, image) in images.iter().enumerate() {
        let col = (index as u32) % cols;
        let row = (index as u32) / cols;
        let fitted = image.resize(tile, tile, FilterType::Lanczos3);
        let (w, h) = fitted.dimensions();
        let x = col * cell + margin + (tile - w) / 2;
        let y = row * cell + margin + (tile - h) / 2;
        imageops::overlay(&mut canvas, &fitted.to_rgba8(), i64::from(x), i64::from(y));
    }
    let height =
        (u64::from(WIDTH) * u64::from(canvas.height()) / u64::from(canvas.width())).max(1) as u32;
    Some(imageops::resize(
        &canvas,
        WIDTH,
        height,
        FilterType::Lanczos3,
    ))
}

/// PNG にエンコードする。
pub fn encode_png(image: &RgbaImage) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    image
        .write_to(&mut buffer, ImageFormat::Png)
        .expect("encoding an in-memory PNG does not fail");
    buffer.into_inner()
}

/// 画像ファイルからバナーの PNG を作る。読めないものとデコードできないものは飛ばす。全部飛ばしたら `None`。
///
/// タイルの大きさは `paths` の数から決め、1 枚ずつ読んでその大きさに縮めて持つ。飛ばしたものがあると、
/// 並べる数が減ってタイルが大きくなるので、縮めた画像が少し拡大される。
pub fn build(paths: &[PathBuf]) -> Option<Vec<u8>> {
    let (cols, _) = tile_geometry(paths.len(), RATIO)?;
    let tile = WIDTH / cols;
    let fitted: Vec<DynamicImage> = paths
        .iter()
        .filter_map(|path| {
            let data = match std::fs::read(path) {
                Ok(data) => data,
                Err(error) => {
                    warn!(path = %path.display(), error = %error, "skipping an image that cannot be read");
                    return None;
                }
            };
            // アニメーションは最初のフレーム。
            let Ok(image) = image::load_from_memory(&data) else {
                warn!(path = %path.display(), bytes = data.len(), "skipping an image that cannot be decoded");
                return None;
            };
            Some(image.resize(tile, tile, FilterType::Lanczos3))
        })
        .collect();
    montage(&fitted).map(|image| encode_png(&image))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::TempDir;

    #[test]
    fn tile_geometry_takes_the_closest_ratio_then_the_fewest_empty_cells() {
        assert_eq!(tile_geometry(0, RATIO), None);
        assert_eq!(tile_geometry(1, RATIO), Some((1, 1)));
        // 4 列 1 行（4:1）は 2 列 2 行（1:1）より 3:1 に近い。
        assert_eq!(tile_geometry(4, RATIO), Some((4, 1)));
        // 6 列 1 行（6:1）と 3 列 2 行（3:2）は同じずれで空き枠も無いので、行の少ない方。
        // 6 列 2 行なら 3:1 ちょうどだが、2 行目が丸ごと空くので試さない。
        assert_eq!(tile_geometry(6, RATIO), Some((6, 1)));
        // 4 列 2 行（2:1）は空き枠が 1 つあるが、空き枠の無い 7 列 1 行（7:1）より 3:1 に近い。
        assert_eq!(tile_geometry(7, RATIO), Some((4, 2)));
        // 8 列 2 行（4:1、空き 0）は、6 列 3 行（2:1、空き 2）より 3:1 に近く、空き枠も少ない。
        assert_eq!(tile_geometry(16, RATIO), Some((8, 2)));
        // 17 列 6 行（2.83:1、空き 2）。
        assert_eq!(tile_geometry(100, RATIO), Some((17, 6)));
    }

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        encode_png(&RgbaImage::from_pixel(width, height, Rgba(color)))
    }

    /// 画素ごとに色の違う画像。縮め方の違いが出力に表れる。
    fn pattern(width: u32, height: u32, seed: u8) -> Vec<u8> {
        encode_png(&RgbaImage::from_fn(width, height, |x, y| {
            Rgba([
                (x * 7) as u8 ^ seed,
                (y * 13) as u8,
                (x * y) as u8,
                255 - seed,
            ])
        }))
    }

    #[test]
    fn montage_fits_images_into_cells_on_a_white_background() {
        let red = image::load_from_memory(&solid(10, 20, [255, 0, 0, 255])).unwrap();
        let blue = image::load_from_memory(&solid(30, 10, [0, 0, 255, 255])).unwrap();
        let image = montage(&[red, blue]).unwrap();
        // 2 個は 2 列 1 行。幅は 1100、高さはセル 1 つ分。
        assert_eq!(image.width(), WIDTH);
        assert_eq!(image.height(), 550);
        // 角は余白（白）。
        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 255, 255]);
        // 左のセルの中央は赤、右のセルの中央は青。
        assert_eq!(image.get_pixel(275, 275).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(825, 275).0, [0, 0, 255, 255]);
        // 縦長の赤はセルの左右に余白が残る（アスペクト比を保つ）。
        assert_eq!(image.get_pixel(60, 275).0, [255, 255, 255, 255]);
        assert!(montage(&[]).is_none());
    }

    #[test]
    fn build_skips_unusable_images_and_returns_png() {
        let dir = TempDir::new();
        std::fs::write(dir.join("green.png"), solid(4, 4, [0, 255, 0, 255])).unwrap();
        std::fs::write(dir.join("broken.png"), b"not an image").unwrap();
        let png = build(&[
            dir.join("green.png"),
            dir.join("broken.png"),
            dir.join("missing.png"),
        ])
        .unwrap();
        assert_eq!(&png[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        let decoded = image::load_from_memory(&png).unwrap();
        // 並べるのは使えた 1 枚だけで、1 列 1 行になる。
        assert_eq!((decoded.width(), decoded.height()), (WIDTH, WIDTH));
        assert!(build(&[dir.join("broken.png"), dir.join("missing.png")]).is_none());
        assert!(build(&[]).is_none());
    }

    #[test]
    fn build_matches_a_montage_of_the_full_size_images() {
        let dir = TempDir::new();
        let images = [
            pattern(10, 20, 0),
            pattern(300, 100, 80),
            pattern(64, 64, 160),
        ];
        let paths: Vec<PathBuf> = images
            .iter()
            .enumerate()
            .map(|(index, data)| {
                let path = dir.join(format!("{index}.png"));
                std::fs::write(&path, data).unwrap();
                path
            })
            .collect();
        let decoded: Vec<DynamicImage> = images
            .iter()
            .map(|data| image::load_from_memory(data).unwrap())
            .collect();
        // 先に縮めても、原寸から並べたものと同じ画像になる。
        assert_eq!(
            build(&paths).unwrap(),
            encode_png(&montage(&decoded).unwrap())
        );
    }
}
