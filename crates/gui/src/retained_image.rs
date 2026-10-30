//! Decoded assets live as long as the cache or a painted element needs them.
//!
//! GPUI's global asset cache retains completed decodes until explicitly removed.
//! Dropping the encoded image alone does not remove its decoded pixels. Each
//! image therefore owns its decode entities, and each painted element leases
//! the entity through element state. The last owner retires the asset and its
//! atlas tiles after window updates unwind. Pending GPUI loads are cancelled
//! by the same removal, so late work cannot repopulate an abandoned entry.

use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

use gpui::{
    App, AppContext as _, Asset, ElementId, Entity, Image, ImageCacheError, ImageSource,
    RenderImage,
};

#[derive(Clone)]
pub(crate) struct RetainedImage(Rc<Inner>);

struct Inner {
    image: Arc<Image>,
    assets: RefCell<HashMap<bool, Entity<Decoded>>>,
}

struct Decoded {
    source: Source,
    output: Option<Arc<RenderImage>>,
}

#[derive(Clone, Hash)]
struct Source {
    image: Arc<Image>,
    sticker: bool,
}

struct Decoder;

impl Asset for Decoder {
    type Source = Source;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(source: Source, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            if source.sticker {
                crate::components::message_bubble::decode_sticker(&source.image.bytes)
                    .map_err(Into::into)
            } else {
                source.image.to_image_data(renderer).map_err(Into::into)
            }
        }
    }
}

impl std::ops::Deref for RetainedImage {
    type Target = Image;

    fn deref(&self) -> &Image {
        &self.0.image
    }
}

impl RetainedImage {
    #[cfg(test)]
    pub(crate) fn decoded(&self, sticker: bool, cx: &App) -> Option<Arc<RenderImage>> {
        self.0
            .assets
            .borrow()
            .get(&sticker)?
            .read(cx)
            .output
            .clone()
    }

    pub(crate) fn new(image: Image) -> Self {
        Self(Rc::new(Inner {
            image: Arc::new(image),
            assets: RefCell::new(HashMap::new()),
        }))
    }

    pub(crate) fn source(&self, sticker: bool) -> ImageSource {
        let owner = self.clone();
        ImageSource::from(move |window: &mut gpui::Window, cx: &mut App| {
            let asset = owner
                .0
                .assets
                .borrow_mut()
                .entry(sticker)
                .or_insert_with(|| {
                    cx.new(|cx| {
                        cx.on_release(|asset: &mut Decoded, cx| {
                            let source = asset.source.clone();
                            let output = asset.output.clone();
                            cx.defer(move |cx| {
                                cx.remove_asset::<Decoder>(&source);
                                if let Some(output) = output {
                                    cx.drop_image(output, None);
                                }
                            });
                        })
                        .detach();
                        Decoded {
                            source: Source {
                                image: owner.0.image.clone(),
                                sticker,
                            },
                            output: None,
                        }
                    })
                })
                .clone();

            // A cached paint can retain atlas coordinates without retaining its
            // ImageSource. Keep the asset with that element state as well.
            window.with_global_id(
                ElementId::named_usize(
                    if sticker {
                        "sticker-asset"
                    } else {
                        "image-asset"
                    },
                    owner.id as usize,
                ),
                |id, window| {
                    window.with_element_state(id, |_: Option<Entity<Decoded>>, _| {
                        ((), asset.clone())
                    });
                },
            );
            asset.update(cx, |asset, cx| {
                let output = window.use_asset::<Decoder>(&asset.source, cx);
                if let Some(Ok(image)) = &output {
                    asset.output = Some(image.clone());
                }
                output
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        IntoElement, ParentElement as _, Render, Styled as _, TestAppContext, Window, div, img,
    };

    struct View(Option<ImageSource>);

    impl Render for View {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .children(self.0.clone().map(|source| img(source).size_full()))
        }
    }

    fn image(index: u8) -> RetainedImage {
        let pixels = image::RgbaImage::from_pixel(1024, 1024, image::Rgba([index, 7, 11, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        RetainedImage::new(Image::from_bytes(
            gpui::ImageFormat::Png,
            bytes.into_inner(),
        ))
    }

    #[gpui::test]
    fn eviction_waits_for_visible_elements_and_reopening_uses_the_same_decode(
        cx: &mut TestAppContext,
    ) {
        let owner = image(1);
        let source = owner.source(false);
        let handle = cx.open_window(gpui::size(gpui::px(40.), gpui::px(40.)), |_, _| {
            View(Some(source))
        });
        let mut window = gpui::VisualTestContext::from_window(handle.into(), cx);
        window.run_until_parked();
        let decoded = window.read(|cx| {
            owner.0.assets.borrow()[&false]
                .read(cx)
                .output
                .clone()
                .unwrap()
        });
        let weak = Arc::downgrade(&decoded);
        let mut probe = RenderImage::new(smallvec::smallvec![image::Frame::new(
            image::RgbaImage::new(1, 1)
        )]);
        probe.id = decoded.id;
        let source = Source {
            image: owner.0.image.clone(),
            sticker: false,
        };
        drop(decoded);

        // Closing while still cached must neither remove nor decode it again.
        handle
            .update(&mut window, |view, _, cx| {
                view.0 = None;
                cx.notify();
            })
            .unwrap();
        window.run_until_parked();
        handle
            .update(&mut window, |view, _, cx| {
                view.0 = Some(owner.source(false));
                cx.notify();
            })
            .unwrap();
        window.run_until_parked();
        assert!(window.read(|cx| {
            Arc::ptr_eq(
                &weak.upgrade().unwrap(),
                owner.0.assets.borrow()[&false]
                    .read(cx)
                    .output
                    .as_ref()
                    .unwrap(),
            )
        }));

        // The cache evicts it while a surface still paints it.
        drop(owner);
        window.run_until_parked();
        assert!(weak.upgrade().is_some());
        assert!(window.read(|cx| cx.has_asset::<Decoder>(&source)));
        assert!(window.update(|window, _| window.has_image_atlas_entry(&probe)));

        handle
            .update(&mut window, |view, _, cx| {
                view.0 = None;
                cx.notify();
            })
            .unwrap();
        window.run_until_parked();
        window.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        window.run_until_parked();
        assert!(!window.read(|cx| cx.has_asset::<Decoder>(&source)));
        assert!(
            weak.upgrade().is_none(),
            "the decoded pixels have no remaining owner"
        );
        assert!(!window.update(|window, _| window.has_image_atlas_entry(&probe)));
    }

    #[gpui::test]
    fn closing_before_decode_finishes_does_not_repopulate_the_asset_cache(cx: &mut TestAppContext) {
        let owner = image(2);
        let source = Source {
            image: owner.0.image.clone(),
            sticker: false,
        };
        let handle = cx.open_window(gpui::size(gpui::px(40.), gpui::px(40.)), |_, _| {
            View(Some(owner.source(false)))
        });
        let mut window = gpui::VisualTestContext::from_window(handle.into(), cx);
        // Start drawing without pumping the background executor to completion.
        window.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        assert!(window.read(|cx| cx.has_asset::<Decoder>(&source)));
        assert!(window.read(|cx| owner.decoded(false, cx).is_none()));
        drop(owner);
        handle
            .update(&mut window, |view, _, cx| {
                view.0 = None;
                cx.notify();
            })
            .unwrap();
        window.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        window.run_until_parked();
        assert!(!window.read(|cx| cx.has_asset::<Decoder>(&source)));
    }

    #[gpui::test]
    fn cached_paint_keeps_pixels_and_atlas_until_the_surface_repaints(cx: &mut TestAppContext) {
        struct Pane {
            source: Option<ImageSource>,
            renders: usize,
        }
        impl Render for Pane {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                self.renders += 1;
                div()
                    .size_full()
                    .children(self.source.clone().map(|source| img(source).size_full()))
            }
        }
        struct Root(Entity<Pane>);
        impl Render for Root {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                self.0
                    .clone()
                    .cached(gpui::StyleRefinement::default().size_full())
            }
        }
        let owner = image(3);
        let handle = cx.open_window(gpui::size(gpui::px(40.), gpui::px(40.)), |_, cx| {
            Root(cx.new(|_| Pane {
                source: Some(owner.source(false)),
                renders: 0,
            }))
        });
        cx.run_until_parked();
        let decoded = cx.read(|cx| owner.decoded(false, cx).unwrap());
        let weak = Arc::downgrade(&decoded);
        let mut probe = RenderImage::new(smallvec::smallvec![image::Frame::new(
            image::RgbaImage::new(1, 1)
        )]);
        probe.id = decoded.id;
        drop(decoded);
        drop(owner);
        let renders = handle
            .update(cx, |root, _, cx| {
                let renders = root.0.read(cx).renders;
                root.0.update(cx, |pane, _| pane.source = None);
                cx.notify();
                renders
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .update(cx, |root, window, cx| {
                assert_eq!(
                    root.0.read(cx).renders,
                    renders,
                    "cached paint must be reused"
                );
                assert!(weak.upgrade().is_some());
                assert!(window.has_image_atlas_entry(&probe));
                root.0.update(cx, |_, cx| cx.notify());
            })
            .unwrap();
        cx.run_until_parked();
        handle.update(cx, |_, _, _| {}).unwrap();
        handle
            .update(cx, |_, window, _| {
                assert!(weak.upgrade().is_none());
                assert!(!window.has_image_atlas_entry(&probe));
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_shared_image_survives_another_window_closing(cx: &mut TestAppContext) {
        let owner = image(4);
        let windows: Vec<_> = (0..2)
            .map(|_| {
                cx.open_window(gpui::size(gpui::px(40.), gpui::px(40.)), |_, _| {
                    View(Some(owner.source(false)))
                })
            })
            .collect();
        cx.run_until_parked();
        let decoded = cx.read(|cx| owner.decoded(false, cx).unwrap());
        let weak = Arc::downgrade(&decoded);
        let mut probe = RenderImage::new(smallvec::smallvec![image::Frame::new(
            image::RgbaImage::new(1, 1)
        )]);
        probe.id = decoded.id;
        drop(decoded);
        drop(owner);
        windows[0]
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        windows[1]
            .update(cx, |view, window, cx| {
                assert!(weak.upgrade().is_some());
                assert!(window.has_image_atlas_entry(&probe));
                view.0 = None;
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        windows[1].update(cx, |_, _, _| {}).unwrap();
        windows[1]
            .update(cx, |_, window, _| {
                assert!(weak.upgrade().is_none());
                assert!(!window.has_image_atlas_entry(&probe));
            })
            .unwrap();
    }

    /// Reproducible native diagnostic, kept out of the ordinary test run.
    /// The legacy arm uses the same decoder without an owner, matching GPUI's
    /// global asset retention before this change. RSS/PSS are observations;
    /// live pixel bytes are counted through weak references to actual outputs.
    #[cfg(target_os = "linux")]
    #[gpui::test]
    #[ignore]
    fn synthetic_image_lifecycle_report(cx: &mut TestAppContext) {
        fn snapshot() -> String {
            let memory = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
            let memory = memory
                .lines()
                .filter(|line| line.starts_with("Rss:") || line.starts_with("Pss:"))
                .collect::<Vec<_>>()
                .join(" ");
            let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
            let fields: Vec<_> = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .collect();
            format!(
                "{memory} cpu_ticks={}",
                fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
            )
        }
        let handle = cx.open_window(gpui::size(gpui::px(40.), gpui::px(40.)), |_, _| View(None));
        let mut window = gpui::VisualTestContext::from_window(handle.into(), cx);
        let arms = match std::env::var("OXIDEZAP_IMAGE_PROFILE_MODE").as_deref() {
            Ok("legacy") => vec![true],
            Ok("owned") => vec![false],
            _ => vec![true, false],
        };
        for legacy in arms {
            let mut cache = std::collections::VecDeque::new();
            let mut sources = Vec::new();
            let mut outputs = Vec::new();
            println!("legacy={legacy} begin {}", snapshot());
            for index in 0..80 {
                let owner = image(index);
                let source = Source {
                    image: owner.0.image.clone(),
                    sticker: false,
                };
                let drawn = if legacy {
                    let source = source.clone();
                    ImageSource::from(move |window: &mut Window, cx: &mut App| {
                        window.use_asset::<Decoder>(&source, cx)
                    })
                } else {
                    owner.source(false)
                };
                handle
                    .update(&mut window, |view, _, cx| {
                        view.0 = Some(drawn);
                        cx.notify();
                    })
                    .unwrap();
                window.run_until_parked();
                let output =
                    window.update(|_, cx| cx.fetch_asset::<Decoder>(&source).unwrap().unwrap());
                outputs.push(Arc::downgrade(&output));
                sources.push(source);
                cache.push_back(owner);
                if cache.len() > 50 {
                    cache.pop_front();
                }
            }
            let live = || {
                outputs
                    .iter()
                    .filter_map(std::sync::Weak::upgrade)
                    .map(|output| output.as_bytes(0).unwrap().len())
                    .sum::<usize>()
            };
            window.run_until_parked();
            window.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            window.run_until_parked();
            println!(
                "legacy={legacy} after_80 live_pixels={} {}",
                live(),
                snapshot()
            );
            assert_eq!(live(), if legacy { 80 } else { 50 } * 1024 * 1024 * 4);
            println!("legacy={legacy} before_redraw {}", snapshot());
            for _ in 0..10_000 {
                window.update(|window, cx| {
                    window.refresh();
                    window.draw(cx).clear(cx);
                });
            }
            println!("legacy={legacy} after_redraw {}", snapshot());
            cache.clear();
            handle
                .update(&mut window, |view, _, cx| {
                    view.0 = None;
                    cx.notify();
                })
                .unwrap();
            window.run_until_parked();
            window.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            window.run_until_parked();
            println!(
                "legacy={legacy} after_close live_pixels={} {}",
                live(),
                snapshot()
            );
            assert_eq!(live(), if legacy { 80 } else { 0 } * 1024 * 1024 * 4);
            // Release the intentionally retained baseline before the next arm.
            window.update(|_, cx| {
                for source in sources {
                    cx.remove_asset::<Decoder>(&source);
                }
            });
            window.run_until_parked();
        }
    }
}
