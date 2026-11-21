//! The background image of the selected beatmap: decoded on a thread of
//! its own, since a large JPEG takes longer than a frame.

use crate::image;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use rsu_gfx::BACKGROUND_SIZE;

/// A decoded background, premultiplied (backgrounds are opaque, so that is
/// what they are anyway), or `None` if the file could not be read.
pub struct Decoded {
    pub path: PathBuf,
    pub image: Option<image::Image>,
}

pub struct Backgrounds {
    requests: Sender<PathBuf>,
    results: Receiver<Decoded>,
}

impl Backgrounds {
    pub fn new() -> Backgrounds {
        let (requests, rx) = channel::<PathBuf>();
        let (tx, results) = channel();
        let spawned = std::thread::Builder::new().name("rsu-background".into()).spawn(move || {
            while let Ok(mut path) = rx.recv() {
                // Only the newest request is still wanted.
                while let Ok(newer) = rx.try_recv() {
                    path = newer;
                }
                let image = image::load(&path, BACKGROUND_SIZE.0 as usize, BACKGROUND_SIZE.1 as usize)
                    .map(|mut image| {
                        for p in image.rgba.chunks_exact_mut(4) {
                            p[3] = 255;
                        }
                        image
                    })
                    .ok();
                if tx.send(Decoded { path, image }).is_err() {
                    break;
                }
            }
        });
        if let Err(e) = spawned {
            eprintln!("rsu: could not start the background loader: {e}");
        }
        Backgrounds { requests, results }
    }

    pub fn request(&self, path: PathBuf) {
        let _ = self.requests.send(path);
    }

    pub fn poll(&self) -> Option<Decoded> {
        self.results.try_recv().ok()
    }
}
