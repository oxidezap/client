//! Atlas lifetime for decoded video frames.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    AppContext as _, ElementId, Entity, Global, ImageId, ImageSource, RenderImage, WeakEntity,
};

struct Texture(Arc<RenderImage>);

#[derive(Default)]
struct Textures(HashMap<ImageId, WeakEntity<Texture>>);

impl Global for Textures {}

pub fn video_image(image: Arc<RenderImage>) -> ImageSource {
    ImageSource::from(move |window: &mut gpui::Window, cx: &mut gpui::App| {
        // Cached paint keeps accessed element state alive, but holds only atlas
        // tile coordinates, not an Arc<RenderImage>. Retire with that state,
        // never with the producer's frame slot or a next-frame callback.
        window.with_global_id(
            ElementId::named_usize("video-texture", image.id.0),
            |id, window| {
                window.with_element_state(id, |state: Option<Entity<Texture>>, _| {
                    let texture = state
                        .or_else(|| {
                            cx.default_global::<Textures>()
                                .0
                                .get(&image.id)
                                .and_then(WeakEntity::upgrade)
                        })
                        .unwrap_or_else(|| {
                            let texture = cx.new(|cx| {
                                cx.on_release(|texture: &mut Texture, cx| {
                                    let image = texture.0.clone();
                                    // All window updates must unwind before None can
                                    // reach every atlas. Another view may have acquired
                                    // this image again before the deferred effect runs.
                                    cx.defer(move |cx| {
                                        let textures = cx.default_global::<Textures>();
                                        if textures
                                            .0
                                            .get(&image.id)
                                            .and_then(WeakEntity::upgrade)
                                            .is_none()
                                        {
                                            textures.0.remove(&image.id);
                                            cx.drop_image(image, None);
                                        }
                                    });
                                })
                                .detach();
                                Texture(image.clone())
                            });
                            cx.default_global::<Textures>()
                                .0
                                .insert(image.id, texture.downgrade());
                            texture
                        });
                    ((), texture)
                });
            },
        );
        Some(Ok(image.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, IntoElement, ParentElement, Render, StyleRefinement, Styled, TestAppContext,
        Window, div, img, px, size,
    };

    struct Pane {
        frame: Option<Arc<RenderImage>>,
        renders: usize,
    }

    impl Render for Pane {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.renders += 1;
            div().size_full().children(
                self.frame
                    .clone()
                    .map(|frame| img(video_image(frame)).size_full()),
            )
        }
    }

    struct Root(Vec<Entity<Pane>>);

    impl Render for Root {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().children(
                self.0
                    .iter()
                    .map(|pane| pane.clone().cached(StyleRefinement::default().size_full())),
            )
        }
    }

    fn image() -> Arc<RenderImage> {
        Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]))
    }

    #[gpui::test]
    fn replaced_video_frames_leave_the_atlas(cx: &mut TestAppContext) {
        let window = cx.open_window(size(px(100.), px(100.)), |_, cx| {
            Root(vec![cx.new(|_| Pane {
                frame: None,
                renders: 0,
            })])
        });
        let frames: Vec<_> = (0..120).map(|_| image()).collect();
        for frame in &frames {
            window
                .update(cx, |root, _, cx| {
                    root.0[0].update(cx, |pane, cx| {
                        pane.frame = Some(frame.clone());
                        cx.notify();
                    });
                })
                .unwrap();
            cx.run_until_parked();
            window.update(cx, |_, _, _| {}).unwrap();
            window
                .update(cx, |_, window, cx| {
                    assert!(window.has_image_atlas_entry(frame));
                    assert_eq!(
                        frames
                            .iter()
                            .filter(|frame| window.has_image_atlas_entry(frame))
                            .count(),
                        1
                    );
                    assert_eq!(cx.default_global::<Textures>().0.len(), 1);
                })
                .unwrap();
        }
        window
            .update(cx, |root, _, cx| {
                root.0.clear();
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        window.update(cx, |_, _, _| {}).unwrap();
        window
            .update(cx, |_, window, cx| {
                let retained = frames
                    .iter()
                    .filter(|frame| window.has_image_atlas_entry(frame))
                    .count();
                assert_eq!(retained, 0, "obsolete video textures remain in the atlas");
                assert!(cx.default_global::<Textures>().0.is_empty());
            })
            .unwrap();
    }

    #[gpui::test]
    fn cached_scene_keeps_texture_until_its_pane_repaints(cx: &mut TestAppContext) {
        let frame = image();
        let window = cx.open_window(size(px(100.), px(100.)), |_, cx| {
            Root(vec![cx.new(|_| Pane {
                frame: Some(frame.clone()),
                renders: 0,
            })])
        });
        cx.run_until_parked();
        let renders = window
            .update(cx, |root, window, cx| {
                assert!(window.has_image_atlas_entry(&frame));
                let renders = root.0[0].read(cx).renders;
                // Clear the producer without invalidating the cached pane.
                root.0[0].update(cx, |pane, _| pane.frame = None);
                cx.notify();
                renders
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |root, window, cx| {
                assert_eq!(
                    root.0[0].read(cx).renders,
                    renders,
                    "test must reuse cached paint"
                );
                assert!(
                    window.has_image_atlas_entry(&frame),
                    "cached scene still needs its tile"
                );
                root.0[0].update(cx, |_, cx| cx.notify());
            })
            .unwrap();
        cx.run_until_parked();
        window.update(cx, |_, _, _| {}).unwrap();
        window
            .update(cx, |_, window, cx| {
                assert!(!window.has_image_atlas_entry(&frame));
                assert!(cx.default_global::<Textures>().0.is_empty());
            })
            .unwrap();
    }

    #[gpui::test]
    fn shared_texture_survives_one_view_releasing_it(cx: &mut TestAppContext) {
        let frame = image();
        let window = cx.open_window(size(px(100.), px(100.)), |_, cx| {
            Root(
                (0..2)
                    .map(|_| {
                        cx.new(|_| Pane {
                            frame: Some(frame.clone()),
                            renders: 0,
                        })
                    })
                    .collect(),
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |root, _, cx| {
                assert_eq!(cx.default_global::<Textures>().0.len(), 1);
                root.0.remove(0);
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |root, window, cx| {
                assert!(window.has_image_atlas_entry(&frame));
                root.0.clear();
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        window.update(cx, |_, _, _| {}).unwrap();
        window
            .update(cx, |_, window, cx| {
                assert!(!window.has_image_atlas_entry(&frame));
                assert!(cx.default_global::<Textures>().0.is_empty());
            })
            .unwrap();
    }

    #[gpui::test]
    fn shared_texture_survives_another_window_closing(cx: &mut TestAppContext) {
        let frame = image();
        let windows: Vec<_> = (0..2)
            .map(|_| {
                cx.open_window(size(px(100.), px(100.)), |_, cx| {
                    Root(vec![cx.new(|_| Pane {
                        frame: Some(frame.clone()),
                        renders: 0,
                    })])
                })
            })
            .collect();
        cx.run_until_parked();
        windows[0]
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        windows[1]
            .update(cx, |root, window, cx| {
                assert!(window.has_image_atlas_entry(&frame));
                root.0.clear();
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        windows[1].update(cx, |_, _, _| {}).unwrap();
        windows[1]
            .update(cx, |_, window, cx| {
                assert!(!window.has_image_atlas_entry(&frame));
                assert!(cx.default_global::<Textures>().0.is_empty());
            })
            .unwrap();
    }
}
