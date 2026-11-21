//! Helpers for safely constructing [`Buffer`]s from a plugin host's audio buffers.

use std::num::NonZeroU32;
use std::ptr::NonNull;

use nice_plug_core::audio_setup::AudioIOLayout;
use nice_plug_core::buffer::Buffer;

/// Buffers created using [`create_buffers`]. At some point the main `Plugin::process()` should
/// probably also take an argument like this instead of main+aux buffers if we also want to provide
/// access to overflowing input channels for e.g. stereo to mono plugins.
pub struct Buffers<'a, 'buffer: 'a> {
    pub main_buffer: &'a mut Buffer<'buffer>,

    // We can't use `AuxiliaryBuffers` here directly because we need different lifetimes for `'a`
    // and `'buffer` while `AuxiliaryBuffers` uses the same lifetime for both.
    pub aux_inputs: &'a mut [Buffer<'buffer>],
    pub aux_outputs: &'a mut [Buffer<'buffer>],
}

/// A helper for safely creating and initializing [`Buffer`]s based on the host's input and output
/// buffers.
pub struct BufferManager {
    // These are the storage backing the fields in `BufferSource`. The wrapper needs to set these
    // values to match the channel pointers provided by the host. If audio buffers are not provided
    // for a bus, then they should be set to `None`. This helper will then copy data to the buffers
    // or fill them with zeroes if there is no data, while also accounting for in-place main IO
    // buffers.
    main_input_channel_pointers: Option<ChannelPointers>,
    main_output_channel_pointers: Option<ChannelPointers>,
    aux_input_channel_pointers: Vec<Option<ChannelPointers>>,
    aux_output_channel_pointers: Vec<Option<ChannelPointers>>,

    /// The backing buffers that will be filled during `create_buffers`. This `'static` lifetime
    /// will be shortened when returning a reference to these buffers in `create_buffers` to match
    /// the function's lifetime.
    main_buffer: Buffer<'static>,
    main_output_discard: Vec<Vec<f32>>,

    aux_input_buffers: Vec<Buffer<'static>>,
    /// Stores the data to back `aux_input_buffers`. We need to copy the host's auxiliary input
    /// buffers to our own first because the `Buffer` API is designed around mutable buffers, and
    /// the host may reuse its input buffers between plugins.
    aux_input_storage: Vec<Vec<Vec<f32>>>,

    aux_output_buffers: Vec<Buffer<'static>>,
    aux_output_discard: Vec<Vec<Vec<f32>>>,
}

// SAFETY: The raw pointers in the `ChannelPointers` fields/vectors are only used as scratch storage
//         inside of the `create_buffers()` function.
unsafe impl Send for BufferManager {}
unsafe impl Sync for BufferManager {}

/// Host data that the plugin's [`Buffer`]s should be created from. Leave these fields as `None`
/// values
#[derive(Debug)]
pub struct BufferSource<'a> {
    pub main_input_channel_pointers: &'a mut Option<ChannelPointers>,
    pub main_output_channel_pointers: &'a mut Option<ChannelPointers>,
    pub aux_input_channel_pointers: &'a mut [Option<ChannelPointers>],
    pub aux_output_channel_pointers: &'a mut [Option<ChannelPointers>],
}

/// Pointers to raw multichannel audio data for this port.
#[derive(Debug, Clone, Copy)]
pub struct ChannelPointers {
    /// A raw pointer to an array of f32 arrays, containing one array for each channel. `ptrs` must
    /// contain (at least) `num_channel` `*const f32`s, and each of those inner arrays must contain
    /// (at least) `num_samples` `f32` values.
    pub ptrs: NonNull<*mut f32>,
    /// The number of audio channels used for this port.
    pub num_channels: usize,
}

impl ChannelPointers {
    /// The caller guarantees that the pointer array covers the declared channels.
    unsafe fn channel(self, index: usize) -> Option<NonNull<f32>> {
        if index >= self.num_channels {
            return None;
        }
        NonNull::new(unsafe { *self.ptrs.as_ptr().add(index) })
    }
}

impl BufferManager {
    /// Initialize managed buffers for a specific audio IO layout. The actual buffers can be set up
    /// using channel pointer data using [`create_buffers()`][Self::create_buffers()].
    pub fn for_audio_io_layout(max_buffer_size: usize, audio_io_layout: AudioIOLayout) -> Self {
        crate::nice_debug_assert!(
            audio_io_layout
                .main_input_channels
                .map(NonZeroU32::get)
                .unwrap_or(0)
                <= audio_io_layout
                    .main_output_channels
                    .map(NonZeroU32::get)
                    .unwrap_or(0),
            "Stereo-to-mono and other many-to-few audio channel configurations are currently not \
             supported"
        );

        // The buffers are preallocated so that `create_buffers()` can be called without having to
        // allocate
        let main_outputs = audio_io_layout
            .main_output_channels
            .map(NonZeroU32::get)
            .unwrap_or(0) as usize;
        let mut main_buffer = Buffer::default();
        unsafe {
            main_buffer.set_slices(0, |output_slices| {
                output_slices.resize_with(main_outputs, || &mut []);
            })
        };

        let mut aux_input_buffers = Vec::with_capacity(audio_io_layout.aux_input_ports.len());
        let mut aux_input_storage = Vec::with_capacity(audio_io_layout.aux_input_ports.len());
        for num_channels in audio_io_layout.aux_input_ports {
            let mut buffer = Buffer::default();
            unsafe {
                buffer.set_slices(0, |slices| {
                    slices.resize_with(num_channels.get() as usize, || &mut []);
                })
            };

            aux_input_buffers.push(buffer);
            aux_input_storage.push(vec![
                vec![0.0; max_buffer_size];
                num_channels.get() as usize
            ]);
        }

        let mut aux_output_buffers = Vec::with_capacity(audio_io_layout.aux_output_ports.len());
        let mut aux_output_discard = Vec::with_capacity(audio_io_layout.aux_output_ports.len());
        for num_channels in audio_io_layout.aux_output_ports {
            let mut buffer = Buffer::default();
            unsafe {
                buffer.set_slices(0, |slices| {
                    slices.resize_with(num_channels.get() as usize, || &mut []);
                })
            };

            aux_output_buffers.push(buffer);
            aux_output_discard.push(vec![
                vec![0.0; max_buffer_size];
                num_channels.get() as usize
            ]);
        }

        Self {
            main_input_channel_pointers: None,
            main_output_channel_pointers: None,
            aux_input_channel_pointers: vec![None; audio_io_layout.aux_input_ports.len()],
            aux_output_channel_pointers: vec![None; audio_io_layout.aux_output_ports.len()],

            main_buffer,
            main_output_discard: vec![vec![0.0; max_buffer_size]; main_outputs],

            aux_input_buffers,
            aux_input_storage,

            aux_output_buffers,
            aux_output_discard,
        }
    }

    /// Initialize the buffers using the host provided buffer pointers and return a reference to the
    /// created buffers that can be passed to `Plugin::process()`. This accounts for in-place main
    /// IO, missing channel pointers, null pointers, and mismatching channel counts. All
    /// uninitialized buffer data (aux outputs, and main output channels with no matching input
    /// channel) are filled with zeroes.
    ///
    /// `sample_offset` and `num_samples` can be used to slice a set of host channel pointers for
    /// sample accurate automation. If any of the outputs are missing because the host hasn't
    /// provided enough channels or outputs, then they will use preallocated discard storage.
    ///
    /// # Safety
    ///
    /// Any provided `ChannelPointers` must point to memory regions that remain valid to read from
    /// or write to for the lifetime of the returned [`Buffers`]. Input and output samples may only
    /// overlap for the corresponding in-place main channel. The requested block length must not
    /// exceed the maximum used to initialize the buffer manager.
    pub unsafe fn create_buffers<'a, 'buffer: 'a>(
        &'a mut self,
        sample_offset: usize,
        num_samples: usize,
        set_buffer_sources: impl FnOnce(&mut BufferSource),
    ) -> Buffers<'a, 'buffer> {
        // Make sure the caller can't forget to unset previously set values
        self.main_input_channel_pointers = None;
        self.main_output_channel_pointers = None;
        self.aux_input_channel_pointers.fill(None);
        self.aux_output_channel_pointers.fill(None);
        set_buffer_sources(&mut BufferSource {
            main_input_channel_pointers: &mut self.main_input_channel_pointers,
            main_output_channel_pointers: &mut self.main_output_channel_pointers,
            aux_input_channel_pointers: &mut self.aux_input_channel_pointers,
            aux_output_channel_pointers: &mut self.aux_output_channel_pointers,
        });

        // Copy inputs to the host's outputs or discard storage, preserving in-place data.
        // Channels without input start with silence.
        unsafe {
            self.main_buffer.set_slices(num_samples, |output_slices| {
                for (index, (output_slice, discard)) in output_slices
                    .iter_mut()
                    .zip(&mut self.main_output_discard)
                    .enumerate()
                {
                    let output = self
                        .main_output_channel_pointers
                        .and_then(|port| port.channel(index));
                    // The returned buffer borrows the manager, and each channel has
                    // its own allocation.
                    *output_slice = match output {
                        Some(output) => std::slice::from_raw_parts_mut(
                            output.as_ptr().add(sample_offset),
                            num_samples,
                        ),
                        None => &mut *(&mut discard[..num_samples] as *mut [f32]),
                    };
                    match self
                        .main_input_channel_pointers
                        .and_then(|port| port.channel(index))
                    {
                        Some(input) if Some(input) == output => {}
                        Some(input) => output_slice.copy_from_slice(std::slice::from_raw_parts(
                            input.as_ptr().add(sample_offset),
                            num_samples,
                        )),
                        None => output_slice.fill(0.0),
                    }
                }
            });
        }

        // Because nice-plug's `Buffer` type is geared around in-place processing, auxiliary inputs
        // need to be copied to our own buffers first (backed by the 'storage' vectors on this
        // object). That way the plugin can modify those buffers like any other buffers.
        for (input_channel_pointers, (input_storage, input_buffer)) in
            self.aux_input_channel_pointers.iter().zip(
                self.aux_input_storage
                    .iter_mut()
                    .zip(self.aux_input_buffers.iter_mut()),
            )
        {
            // Resize every channel, including silent ones. The storage was allocated
            // for the maximum block size during activation.
            for (index, channel) in input_storage.iter_mut().enumerate() {
                debug_assert!(num_samples <= channel.capacity());
                channel.resize(num_samples, 0.0);
                let input = unsafe { input_channel_pointers.and_then(|port| port.channel(index)) };
                if let Some(input) = input {
                    channel.copy_from_slice(unsafe {
                        std::slice::from_raw_parts(input.as_ptr().add(sample_offset), num_samples)
                    });
                } else {
                    channel.fill(0.0);
                }
            }
            unsafe {
                input_buffer.set_slices(num_samples, |input_slices| {
                    // Since we initialized both `input_buffer` and `input_storage` this invariant
                    // should never fail unless we made an error ourselves
                    debug_assert_eq!(input_slices.len(), input_storage.len());

                    for (channel_slice, channel_storage) in
                        input_slices.iter_mut().zip(input_storage.iter_mut())
                    {
                        // SAFETY: `channel_storage` is no longer used accessed directly after this
                        *channel_slice = &mut *(channel_storage.as_mut_slice() as *mut [f32]);
                    }
                });
            }
        }

        // The auxiliary output buffers can point directly to the host's buffers. This logic is the
        // same as the main outputs, minus the copying of input samples.
        for ((output_channel_pointers, output_buffer), discard_storage) in self
            .aux_output_channel_pointers
            .iter()
            .zip(self.aux_output_buffers.iter_mut())
            .zip(&mut self.aux_output_discard)
        {
            unsafe {
                output_buffer.set_slices(num_samples, |output_slices| {
                    for (index, (output_slice, discard)) in
                        output_slices.iter_mut().zip(discard_storage).enumerate()
                    {
                        *output_slice =
                            match output_channel_pointers.and_then(|port| port.channel(index)) {
                                Some(output) => std::slice::from_raw_parts_mut(
                                    output.as_ptr().add(sample_offset),
                                    num_samples,
                                ),
                                None => &mut *(&mut discard[..num_samples] as *mut [f32]),
                            };
                        output_slice.fill(0.0);
                    }
                });
            }
        }

        // SAFETY: The 'static lifetimes on the objects are needed so we can store the buffers.
        //         Their actual lifetimes are `'a`, so we need to shrink them here. The contents are
        //         valid for as long as the returned object is borrowed.
        unsafe {
            std::mem::transmute::<Buffers<'a, 'static>, Buffers<'a, 'buffer>>(Buffers {
                main_buffer: &mut self.main_buffer,
                aux_inputs: &mut self.aux_input_buffers,
                aux_outputs: &mut self.aux_output_buffers,
            })
        }
    }
}

#[cfg(any(miri, test))]
mod miri {
    use nice_plug_core::audio_setup::{PortNames, new_nonzero_u32};

    use super::*;

    const BUFFER_SIZE: usize = 512;
    const NUM_MAIN_INPUT_CHANNELS: usize = 1;
    const NUM_MAIN_OUTPUT_CHANNELS: usize = 2;

    const NUM_AUX_CHANNELS: usize = 2;
    const NUM_AUX_PORTS: usize = 2;

    const AUDIO_IO_LAYOUT: AudioIOLayout = AudioIOLayout {
        main_input_channels: Some(new_nonzero_u32(NUM_MAIN_INPUT_CHANNELS as u32)),
        main_output_channels: Some(new_nonzero_u32(NUM_MAIN_OUTPUT_CHANNELS as u32)),
        aux_input_ports: &[new_nonzero_u32(NUM_AUX_CHANNELS as u32); NUM_AUX_PORTS],
        aux_output_ports: &[new_nonzero_u32(NUM_AUX_CHANNELS as u32); NUM_AUX_PORTS],
        names: PortNames::const_default(),
    };

    fn port(pointers: &mut [*mut f32]) -> ChannelPointers {
        ChannelPointers {
            ptrs: NonNull::new(pointers.as_mut_ptr()).unwrap(),
            num_channels: pointers.len(),
        }
    }

    #[test]
    fn short_main_output_copies_only_available_channels() {
        let mut input = [[1.0, 2.0], [3.0, 4.0]];
        let mut output = [5.0, 6.0];
        let mut inputs = input.each_mut().map(|samples| samples.as_mut_ptr());
        let mut outputs = [output.as_mut_ptr()];
        let mut manager = BufferManager::for_audio_io_layout(2, AUDIO_IO_LAYOUT);
        let buffers = unsafe {
            manager.create_buffers(0, 2, |sources| {
                *sources.main_input_channel_pointers = Some(port(&mut inputs));
                *sources.main_output_channel_pointers = Some(port(&mut outputs));
            })
        };
        assert_eq!(buffers.main_buffer.as_slice()[0], [1.0, 2.0]);
        assert_eq!(buffers.main_buffer.as_slice()[1], [3.0, 4.0]);
        for samples in buffers.main_buffer.iter_samples() {
            for sample in samples {
                *sample += 1.0;
            }
        }
        assert_eq!(buffers.main_buffer.as_slice()[1], [4.0, 5.0]);
        assert_eq!(output, [2.0, 3.0]);
        assert_eq!(input, [[1.0, 2.0], [3.0, 4.0]]);
    }

    #[test]
    fn output_starts_silent_without_input_and_ignores_extra_channels() {
        let mut output = [[9.0; 4]; 3];
        let mut outputs = output.each_mut().map(|samples| samples.as_mut_ptr());
        let mut manager = BufferManager::for_audio_io_layout(4, AUDIO_IO_LAYOUT);
        let mut missing_output = [std::ptr::null_mut()];
        let buffers = unsafe {
            manager.create_buffers(1, 2, |sources| {
                *sources.main_output_channel_pointers = Some(port(&mut outputs));
                sources.aux_output_channel_pointers[0] = Some(port(&mut missing_output));
            })
        };
        let channels = buffers.main_buffer.as_slice();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0], [0.0, 0.0]);
        assert_eq!(channels[1], [0.0, 0.0]);
        for buffer in buffers.aux_outputs {
            assert_eq!(buffer.as_slice(), [&[0.0; 2][..]; 2]);
            for samples in buffer.iter_samples() {
                for (index, sample) in samples.into_iter().enumerate() {
                    *sample = index as f32 + 1.0;
                }
            }
            assert_eq!(buffer.as_slice()[0], [1.0; 2]);
            assert_eq!(buffer.as_slice()[1], [2.0; 2]);
        }
        assert_eq!(
            output,
            [[9.0, 0.0, 0.0, 9.0], [9.0, 0.0, 0.0, 9.0], [9.0; 4]]
        );
    }

    #[test]
    fn silent_auxiliary_channels_follow_the_current_block() {
        let mut input = [1.0, 2.0, 3.0, 4.0];
        let mut manager = BufferManager::for_audio_io_layout(4, AUDIO_IO_LAYOUT);
        let mut inputs = [input.as_mut_ptr()];
        {
            let buffers = unsafe {
                manager.create_buffers(0, 4, |sources| {
                    sources.aux_input_channel_pointers[0] = Some(port(&mut inputs));
                })
            };
            let channels = buffers.aux_inputs[0].as_slice();
            assert_eq!(channels[0], [1.0, 2.0, 3.0, 4.0]);
            assert_eq!(channels[1], [0.0; 4]);
        }
        let buffers = unsafe { manager.create_buffers(2, 2, |_| {}) };
        for buffer in buffers.aux_inputs {
            for channel in buffer.as_slice() {
                assert_eq!(*channel, [0.0; 2]);
            }
        }
    }

    #[test]
    fn buffer_io() {
        // This works very similarly to the standalone CPAL and dummy backends
        let mut main_io_storage = vec![vec![0.0f32; BUFFER_SIZE]; NUM_MAIN_OUTPUT_CHANNELS];
        let mut aux_input_storage =
            vec![vec![vec![0.0f32; BUFFER_SIZE]; NUM_AUX_CHANNELS]; NUM_AUX_PORTS];
        let mut aux_output_storage =
            vec![vec![vec![0.0f32; BUFFER_SIZE]; NUM_AUX_CHANNELS]; NUM_AUX_PORTS];

        let mut main_io_channel_pointers: Vec<*mut f32> = main_io_storage
            .iter_mut()
            .map(|channel_slice| channel_slice.as_mut_ptr())
            .collect();
        let mut aux_input_channel_pointers: Vec<Vec<*mut f32>> = aux_input_storage
            .iter_mut()
            .map(|aux_input_storage| {
                aux_input_storage
                    .iter_mut()
                    .map(|channel_slice| channel_slice.as_mut_ptr())
                    .collect()
            })
            .collect();
        let mut aux_output_channel_pointers: Vec<Vec<*mut f32>> = aux_output_storage
            .iter_mut()
            .map(|aux_output_storage| {
                aux_output_storage
                    .iter_mut()
                    .map(|channel_slice| channel_slice.as_mut_ptr())
                    .collect()
            })
            .collect();

        // The actual buffer management here works the same as in the JACK backend. See that
        // implementation for more information.
        let mut buffer_manager = BufferManager::for_audio_io_layout(BUFFER_SIZE, AUDIO_IO_LAYOUT);
        let buffers = unsafe {
            buffer_manager.create_buffers(0, BUFFER_SIZE, |buffer_sources| {
                *buffer_sources.main_output_channel_pointers = Some(ChannelPointers {
                    ptrs: NonNull::new(main_io_channel_pointers.as_mut_ptr()).unwrap(),
                    num_channels: main_io_channel_pointers.len(),
                });
                *buffer_sources.main_input_channel_pointers = Some(ChannelPointers {
                    ptrs: NonNull::new(main_io_channel_pointers.as_mut_ptr()).unwrap(),
                    num_channels: NUM_MAIN_INPUT_CHANNELS.min(main_io_channel_pointers.len()),
                });

                for (input_source_channel_pointers, input_channel_pointers) in buffer_sources
                    .aux_input_channel_pointers
                    .iter_mut()
                    .zip(aux_input_channel_pointers.iter_mut())
                {
                    *input_source_channel_pointers = Some(ChannelPointers {
                        ptrs: NonNull::new(input_channel_pointers.as_mut_ptr()).unwrap(),
                        num_channels: input_channel_pointers.len(),
                    });
                }

                for (output_source_channel_pointers, output_channel_pointers) in buffer_sources
                    .aux_output_channel_pointers
                    .iter_mut()
                    .zip(aux_output_channel_pointers.iter_mut())
                {
                    *output_source_channel_pointers = Some(ChannelPointers {
                        ptrs: NonNull::new(output_channel_pointers.as_mut_ptr()).unwrap(),
                        num_channels: output_channel_pointers.len(),
                    });
                }
            })
        };

        for channel_samples in buffers
            .main_buffer
            .iter_samples()
            .chain(
                buffers
                    .aux_inputs
                    .iter_mut()
                    .flat_map(|buffer| buffer.iter_samples()),
            )
            .chain(
                buffers
                    .aux_outputs
                    .iter_mut()
                    .flat_map(|buffer| buffer.iter_samples()),
            )
        {
            for sample in channel_samples {
                *sample += 1.0;
            }
        }

        // These checks are fine due to stacked borrows even without explicitly dropping `buffers`.
        // If we were to access `buffers` again after this miri would trigger an error.
        for channel in main_io_storage
            .iter()
            .chain(aux_output_storage.iter().flat_map(|storage| storage.iter()))
        {
            for sample in channel {
                assert!(*sample == 1.0);
            }
        }

        for channel in aux_input_storage.iter().flat_map(|storage| storage.iter()) {
            for sample in channel {
                assert!(*sample == 0.0);
            }
        }
    }
}
