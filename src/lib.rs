//! An audio sample rate conversion library for Rust.
//!
//! This library provides resamplers to process audio in chunks.
//! The ratio between input and output sample rates is completely free.
//! Implementations are available that accept a fixed length input
//! while returning a variable length output, and vice versa.
//! The resampling is based on band-limited interpolation using sinc
//! interpolation filters. The sinc interpolation upsamples by an adjustable factor,
//! and then the new sample points are calculated by interpolating between these points.
//!
//! ## Example
//! Resample an audio file from 44100 to 48000 Hz.
//! This code is taken from the "fixedin64" example.
//! The functions "read_frames" and "write_frames" are simple
//! helpers that are used to read and write audio data from/to buffers.
//! See the example source for details.
//! ```
//! let mut resampler = SincFixedIn::<f64>::new(
//!     fs_out as f32 / fs_in as f32,
//!     256,
//!     0.95,
//!     160,
//!     Interpolation::Nearest,
//!     1024,
//!     2,
//! );
//!
//! for _chunk in 0..num_chunks {
//!     let waves_in = read_frames(&mut f_in, 1024, 2);
//!     let waves_out = resampler.process(&waves_in).unwrap();
//!     write_frames(waves_out, &mut f_out, 2);
//! }
//! ```
//!
//! ## Compatibility
//!
//! The `camillaresampler` crate only depend on the `num` crate and should work with any rustc version that crate supports.

use num::traits::Float;
use std::error;
use std::fmt;

type Res<T> = Result<T, Box<dyn error::Error>>;

/// Custom error returned by resamplers
#[derive(Debug)]
pub struct ResamplerError {
    desc: String,
}

impl fmt::Display for ResamplerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.desc)
    }
}

impl error::Error for ResamplerError {
    fn description(&self) -> &str {
        &self.desc
    }
}

impl ResamplerError {
    pub fn new(desc: &str) -> Self {
        ResamplerError {
            desc: desc.to_owned(),
        }
    }
}

/// Interpolation methods that can be selected.
pub enum Interpolation {
    /// Cubic polynomial interpolation. Best for asynchronous resampling
    /// but requires calculating four points per output point.
    Cubic,
    /// Linear interpolation. About a factor 2 faster than Cubic, but is less accurate.
    Linear,
    /// No interpolation, just pick nearest point. Suitable for synchronous resampling
    /// if the resampling ration can be expressed as a fraction, for example 48000/44100 = 160/147
    Nearest,
}

/// A resampler that accepts a fixed number of audio chunks for input
/// and returns a variable number of frames.
///
/// The resampling is done by creating a number of intermediate points (defined by upsample_factor)
/// by sinc interpolation. The new samples are then calculated by interpolating between these points.
pub struct SincFixedIn<T: Float> {
    nbr_channels: usize,
    chunk_size: usize,
    upsample_factor: usize,
    last_index: f64,
    resample_ratio: f32,
    resample_ratio_original: f32,
    sinc_len: usize,
    sincs: Vec<Vec<T>>,
    buffer: Vec<Vec<T>>,
    interpolation: Interpolation,
}

/// A resampler that return a fixed number of audio chunks.
/// The number of input frames required is given by the frames_needed function.
///
/// The resampling is done by creating a number of intermediate points (defined by upsample_factor)
/// by sinc interpolation. The new samples are then calculated by interpolating between these points.
pub struct SincFixedOut<T: Float> {
    nbr_channels: usize,
    chunk_size: usize,
    needed_input_size: usize,
    upsample_factor: usize,
    last_index: f64,
    current_buffer_fill: usize,
    resample_ratio: f32,
    resample_ratio_original: f32,
    sinc_len: usize,
    sincs: Vec<Vec<T>>,
    buffer: Vec<Vec<T>>,
    interpolation: Interpolation,
}

/// A resampler that us used to resample a chunk of audio to a new sample rate.
/// The rate can be adjusted as required.
pub trait Resampler<T: Float> {
    /// Resample a chunk of audio. Input and output data is stored in a vector,
    /// where each element contains a vector with all samples for a single channel.
    fn process(&mut self, wave_in: &[Vec<T>]) -> Res<Vec<Vec<T>>>;

    /// Update the resample ratio. New value must be within +-10% of the original one.
    fn set_resample_ratio(&mut self, new_ratio: f32) -> Res<()>;

    /// Query for the number of frames needed for the next call to "process".
    fn nbr_frames_needed(&self) -> usize;
}

impl<T: Float> Resampler<T> for SincFixedIn<T> {
    /// Resample a chunk of audio. The input length is fixed, and the output varies in length.
    /// # Errors
    ///
    /// The function returns an error if the length of the input data is not equal
    /// to the number of channels and chunk size defined when creating the instance.
    fn process(&mut self, wave_in: &[Vec<T>]) -> Res<Vec<Vec<T>>> {
        if wave_in.len() != self.nbr_channels {
            return Err(Box::new(ResamplerError::new(
                "Wrong number of channels in input",
            )));
        }
        if wave_in[0].len() != self.chunk_size {
            return Err(Box::new(ResamplerError::new(
                "Wrong number of frames in input",
            )));
        }
        let end_idx = self.chunk_size as isize - (self.sinc_len as isize + 1);
        //update buffer with new data
        for wav in self.buffer.iter_mut() {
            for idx in 0..(2 * self.sinc_len) {
                wav[idx] = wav[idx + self.chunk_size];
            }
        }
        for (chan, wav) in wave_in.iter().enumerate() {
            for (idx, sample) in wav.iter().enumerate() {
                self.buffer[chan][idx + 2 * self.sinc_len] = *sample;
            }
        }

        let mut idx = self.last_index;
        let t_ratio = 1.0 / self.resample_ratio as f64;

        let mut wave_out =
            vec![
                vec![T::zero(); (self.chunk_size as f32 * self.resample_ratio + 10.0) as usize];
                self.nbr_channels
            ];
        let mut n = 0;

        match self.interpolation {
            Interpolation::Cubic => {
                let mut points = vec![T::zero(); 4];
                let mut nearest = vec![(0isize, 0isize); 4];
                while idx < end_idx as f64 {
                    idx += t_ratio;
                    get_nearest_times_4(idx, self.upsample_factor as isize, &mut nearest);
                    let frac = idx * self.upsample_factor as f64
                        - (idx * self.upsample_factor as f64).floor();
                    let frac_offset = T::from(frac).unwrap();
                    for (chan, buf) in self.buffer.iter().enumerate() {
                        for (n, p) in nearest.iter().zip(points.iter_mut()) {
                            *p = get_sinc_interpolated(
                                &buf,
                                &self.sincs,
                                (n.0 + 2 * self.sinc_len as isize) as usize,
                                n.1 as usize,
                            );
                        }
                        wave_out[chan][n] = interp_cubic(frac_offset, &points);
                    }
                    n += 1;
                }
            }
            Interpolation::Linear => {
                let mut points = vec![T::zero(); 2];
                let mut nearest = vec![(0isize, 0isize); 2];
                while idx < end_idx as f64 {
                    idx += t_ratio;
                    get_nearest_times_2(idx, self.upsample_factor as isize, &mut nearest);
                    let frac = idx * self.upsample_factor as f64
                        - (idx * self.upsample_factor as f64).floor();
                    let frac_offset = T::from(frac).unwrap();
                    for (chan, buf) in self.buffer.iter().enumerate() {
                        for (n, p) in nearest.iter().zip(points.iter_mut()) {
                            *p = get_sinc_interpolated(
                                &buf,
                                &self.sincs,
                                (n.0 + 2 * self.sinc_len as isize) as usize,
                                n.1 as usize,
                            );
                        }
                        wave_out[chan][n] = interp_lin(frac_offset, &points);
                    }
                    n += 1;
                }
            }
            Interpolation::Nearest => {
                let mut point;
                let mut nearest;
                while idx < end_idx as f64 {
                    idx += t_ratio;
                    nearest = get_nearest_time(idx, self.upsample_factor as isize);
                    for (chan, buf) in self.buffer.iter().enumerate() {
                        point = get_sinc_interpolated(
                            &buf,
                            &self.sincs,
                            (nearest.0 + 2 * self.sinc_len as isize) as usize,
                            nearest.1 as usize,
                        );
                        wave_out[chan][n] = point;
                    }
                    n += 1;
                }
            }
        }

        // store last index for next iteration
        self.last_index = idx - self.chunk_size as f64;
        for w in wave_out.iter_mut() {
            w.truncate(n);
        }
        Ok(wave_out)
    }

    /// Update the resample ratio. New value must be within +-10% of the original one
    fn set_resample_ratio(&mut self, new_ratio: f32) -> Res<()> {
        if (new_ratio / self.resample_ratio_original > 0.9)
            && (new_ratio / self.resample_ratio_original < 1.1)
        {
            self.resample_ratio = new_ratio;
            Ok(())
        } else {
            Err(Box::new(ResamplerError::new(
                "New resample ratio is too far off from original",
            )))
        }
    }

    /// Query for the number of frames needed for the next call to "process".
    /// Will always return the chunk_size defined when creating the instance.
    fn nbr_frames_needed(&self) -> usize {
        self.chunk_size
    }
}

impl<T: Float> SincFixedIn<T> {
    /// Create a new SincFixedIn
    ///
    /// Parameters are:
    /// - resample_ratio: ratio of output and input sample rates
    /// - sinc_len: length of the windowed sinc interpolation filters
    /// - f_cutoff: relative cutoff frequency, to the smaller one of fs_in/2 or fs_out/2
    /// - interpolation: interpolation type
    /// - chunk_size: size of input data in frames
    /// - nbr_channels: number of channels in input/output
    pub fn new(
        resample_ratio: f32,
        sinc_len: usize,
        f_cutoff: f32,
        upsample_factor: usize,
        interpolation: Interpolation,
        chunk_size: usize,
        nbr_channels: usize,
    ) -> Self {
        let sinc_cutoff = if resample_ratio >= 0.0 {
            f_cutoff
        } else {
            f_cutoff * resample_ratio
        };
        let sincs = make_sincs(sinc_len, upsample_factor, sinc_cutoff);
        let buffer = vec![vec![T::zero(); chunk_size + 2 * sinc_len]; nbr_channels];
        SincFixedIn {
            nbr_channels,
            chunk_size,
            upsample_factor,
            last_index: -(sinc_len as f64),
            resample_ratio,
            resample_ratio_original: resample_ratio,
            sinc_len,
            sincs,
            buffer,
            interpolation,
        }
    }
}

impl<T: Float> SincFixedOut<T> {
    /// Create a new SincFixedOut
    ///
    /// Parameters are:
    /// - resample_ratio: ratio of output and input sample rates
    /// - sinc_len: length of the windowed sinc interpolation filters
    /// - f_cutoff: relative cutoff frequency, to the smaller one of fs_in/2 or fs_out/2
    /// - interpolation: interpolation type
    /// - chunk_size: size of input data in frames
    /// - nbr_channels: number of channels in input/output
    pub fn new(
        resample_ratio: f32,
        sinc_len: usize,
        f_cutoff: f32,
        upsample_factor: usize,
        interpolation: Interpolation,
        chunk_size: usize,
        nbr_channels: usize,
    ) -> Self {
        let sinc_cutoff = if resample_ratio >= 0.0 {
            f_cutoff
        } else {
            f_cutoff * resample_ratio
        };
        let sincs = make_sincs(sinc_len, upsample_factor, sinc_cutoff);
        let needed_input_size = (chunk_size as f32 / resample_ratio).ceil() as usize + 1;
        let buffer = vec![vec![T::zero(); 3 * needed_input_size / 2 + 2 * sinc_len]; nbr_channels];
        SincFixedOut {
            nbr_channels,
            chunk_size,
            needed_input_size,
            upsample_factor,
            last_index: -(sinc_len as f64),
            current_buffer_fill: needed_input_size,
            resample_ratio,
            resample_ratio_original: resample_ratio,
            sinc_len,
            sincs,
            buffer,
            interpolation,
        }
    }
}

impl<T: Float> Resampler<T> for SincFixedOut<T> {
    /// Query for the number of frames needed for the next call to "process".
    fn nbr_frames_needed(&self) -> usize {
        self.needed_input_size
    }

    /// Update the resample ratio. New value must be within +-10% of the original one
    fn set_resample_ratio(&mut self, new_ratio: f32) -> Res<()> {
        if (new_ratio / self.resample_ratio_original > 0.9)
            && (new_ratio / self.resample_ratio_original < 1.1)
        {
            self.resample_ratio = new_ratio;
            self.needed_input_size =
                (self.chunk_size as f32 / self.resample_ratio).ceil() as usize + 1;
            Ok(())
        } else {
            Err(Box::new(ResamplerError::new(
                "New resample ratio is too far off from original",
            )))
        }
    }

    /// Resample a chunk of audio. The required input length is provided by
    /// the "nbr_frames_required" function, and the output length is fixed.
    /// # Errors
    ///
    /// The function returns an error if the length of the input data is not
    /// equal to the number of channels defined when creating the instance,
    /// and the number of audio frames given by "nbr_frames"required".
    fn process(&mut self, wave_in: &[Vec<T>]) -> Res<Vec<Vec<T>>> {
        //update buffer with new data
        if wave_in.len() != self.nbr_channels {
            return Err(Box::new(ResamplerError::new(
                "Wrong number of channels in input",
            )));
        }
        if wave_in[0].len() != self.needed_input_size {
            return Err(Box::new(ResamplerError::new(
                "Wrong number of frames in input",
            )));
        }
        for wav in self.buffer.iter_mut() {
            for idx in 0..(2 * self.sinc_len) {
                wav[idx] = wav[idx + self.current_buffer_fill];
            }
        }
        self.current_buffer_fill = wave_in[0].len();
        for (chan, wav) in wave_in.iter().enumerate() {
            for (idx, sample) in wav.iter().enumerate() {
                self.buffer[chan][idx + 2 * self.sinc_len] = *sample;
            }
        }

        let mut idx = self.last_index;
        let t_ratio = 1.0 / self.resample_ratio as f64;

        let mut wave_out = vec![vec![T::zero(); self.chunk_size]; self.nbr_channels];

        match self.interpolation {
            Interpolation::Cubic => {
                let mut points = vec![T::zero(); 4];
                let mut nearest = vec![(0isize, 0isize); 4];
                for n in 0..self.chunk_size {
                    idx += t_ratio;
                    get_nearest_times_4(idx, self.upsample_factor as isize, &mut nearest);
                    let frac = idx * self.upsample_factor as f64
                        - (idx * self.upsample_factor as f64).floor();
                    let frac_offset = T::from(frac).unwrap();
                    for (chan, buf) in self.buffer.iter().enumerate() {
                        for (n, p) in nearest.iter().zip(points.iter_mut()) {
                            *p = get_sinc_interpolated(
                                &buf,
                                &self.sincs,
                                (n.0 + 2 * self.sinc_len as isize) as usize,
                                n.1 as usize,
                            );
                        }
                        wave_out[chan][n] = interp_cubic(frac_offset, &points);
                    }
                }
            }
            Interpolation::Linear => {
                let mut points = vec![T::zero(); 2];
                let mut nearest = vec![(0isize, 0isize); 2];
                for n in 0..self.chunk_size {
                    idx += t_ratio;
                    get_nearest_times_2(idx, self.upsample_factor as isize, &mut nearest);
                    let frac = idx * self.upsample_factor as f64
                        - (idx * self.upsample_factor as f64).floor();
                    let frac_offset = T::from(frac).unwrap();
                    for (chan, buf) in self.buffer.iter().enumerate() {
                        for (n, p) in nearest.iter().zip(points.iter_mut()) {
                            *p = get_sinc_interpolated(
                                &buf,
                                &self.sincs,
                                (n.0 + 2 * self.sinc_len as isize) as usize,
                                n.1 as usize,
                            );
                        }
                        wave_out[chan][n] = interp_lin(frac_offset, &points);
                    }
                }
            }
            Interpolation::Nearest => {
                let mut point;
                let mut nearest;
                for n in 0..self.chunk_size {
                    idx += t_ratio;
                    nearest = get_nearest_time(idx, self.upsample_factor as isize);
                    for (chan, buf) in self.buffer.iter().enumerate() {
                        point = get_sinc_interpolated(
                            &buf,
                            &self.sincs,
                            (nearest.0 + 2 * self.sinc_len as isize) as usize,
                            nearest.1 as usize,
                        );
                        wave_out[chan][n] = point;
                    }
                }
            }
        }

        // store last index for next iteration
        self.last_index = idx - self.current_buffer_fill as f64;
        self.needed_input_size = (self.needed_input_size as isize
            + self.last_index.round() as isize
            + self.sinc_len as isize) as usize;
        Ok(wave_out)
    }
}

/// Helper function. Standard Blackman-Harris window
fn blackman_harris<T: Float>(npoints: usize) -> Vec<T> {
    let mut window = vec![T::zero(); npoints];
    let pi2 = T::from(2.0 * std::f64::consts::PI).unwrap();
    let pi4 = T::from(4.0 * std::f64::consts::PI).unwrap();
    let pi6 = T::from(6.0 * std::f64::consts::PI).unwrap();
    let np_f = T::from(npoints).unwrap();
    let a = T::from(0.35875).unwrap();
    let b = T::from(0.48829).unwrap();
    let c = T::from(0.14128).unwrap();
    let d = T::from(0.01168).unwrap();
    for (x, item) in window.iter_mut().enumerate() {
        let x_float = T::from(x).unwrap();
        *item = a - b * (pi2 * x_float / np_f).cos() + c * (pi4 * x_float / np_f).cos()
            - d * (pi6 * x_float / np_f).cos();
    }
    window
}

/// Helper function: sinc(x) = sin(pi*x)/(pi*x)
fn sinc<T: Float>(value: T) -> T {
    let pi = T::from(std::f64::consts::PI).unwrap();
    if value == T::zero() {
        T::from(1.0).unwrap()
    } else {
        (T::from(value).unwrap() * pi).sin() / (T::from(value).unwrap() * pi)
    }
}

/// Helper function. Make a set of windowed sincs.  
fn make_sincs<T: Float>(npoints: usize, factor: usize, f_cutoff: f32) -> Vec<Vec<T>> {
    let totpoints = (npoints * factor) as isize;
    let mut y = Vec::with_capacity(totpoints as usize);
    let window = blackman_harris::<T>(totpoints as usize);
    for x in 0..totpoints {
        let val = window[x as usize]
            * window[x as usize]
            * sinc(
                T::from(x - totpoints / 2).unwrap() * T::from(f_cutoff).unwrap()
                    / T::from(factor).unwrap(),
            );
        y.push(val);
    }
    let mut sincs = vec![vec![T::zero(); npoints]; factor];
    for p in 0..npoints {
        for n in 0..factor {
            sincs[factor - n - 1][p] = y[factor * p + n];
        }
    }
    sincs
}

/// Perform cubic polynomial interpolation to get value at x.
/// Input points are assumed to be at x = -1, 0, 1, 2
fn interp_cubic<T: Float>(x: T, yvals: &[T]) -> T {
    let a0 = yvals[1];
    let a1 = -T::from(1.0 / 3.0).unwrap() * yvals[0] - T::from(0.5).unwrap() * yvals[1] + yvals[2]
        - T::from(1.0 / 6.0).unwrap() * yvals[3];
    let a2 = T::from(1.0 / 2.0).unwrap() * (yvals[0] + yvals[2]) - yvals[1];
    let a3 = T::from(1.0 / 2.0).unwrap() * (yvals[1] - yvals[2])
        + T::from(1.0 / 6.0).unwrap() * (yvals[3] - yvals[0]);
    a0 + a1 * x + a2 * x.powi(2) + a3 * x.powi(3)
}

/// Linear interpolation between two points at x=0 and x=1
fn interp_lin<T: Float>(x: T, yvals: &[T]) -> T {
    (T::one() - x) * yvals[0] + x * yvals[1]
}

/// Calculate the scalar produt of an input wave and the selected sinc filter
fn get_sinc_interpolated<T: Float>(
    wave: &[T],
    sincs: &[Vec<T>],
    index: usize,
    subindex: usize,
) -> T {
    wave.iter()
        .skip(index)
        .take(sincs[subindex].len())
        .zip(sincs[subindex].iter())
        .fold(T::zero(), |acc, (x, y)| acc.add(*x * *y))
}

/// Get the two nearest time points for time t in format (index, subindex)
fn get_nearest_times_2<T: Float>(t: T, factor: isize, points: &mut [(isize, isize)]) {
    let mut index = t.floor().to_isize().unwrap();
    let mut subindex = ((t - t.floor()) * T::from(factor).unwrap())
        .floor()
        .to_isize()
        .unwrap();
    points[0] = (index, subindex);
    subindex += 1;
    if subindex >= factor {
        subindex -= factor;
        index += 1;
    }
    points[1] = (index, subindex);
}

/// Get the four nearest time points for time t in format (index, subindex).
fn get_nearest_times_4<T: Float>(t: T, factor: isize, points: &mut [(isize, isize)]) {
    let start = t.floor().to_isize().unwrap();
    let frac = ((t - t.floor()) * T::from(factor).unwrap())
        .floor()
        .to_isize()
        .unwrap();
    let mut index;
    let mut subindex;
    for (idx, sub) in (-1..3).enumerate() {
        index = start;
        subindex = frac + sub;
        if subindex < 0 {
            subindex += factor;
            index -= 1;
        } else if subindex >= factor {
            subindex -= factor;
            index += 1;
        }
        points[idx] = (index, subindex);
    }
}

/// Get the nearest time point for time t in format (index, subindex).
fn get_nearest_time<T: Float>(t: T, factor: isize) -> (isize, isize) {
    let mut index = t.floor().to_isize().unwrap();
    let mut subindex = ((t - t.floor()) * T::from(factor).unwrap())
        .round()
        .to_isize()
        .unwrap();
    if subindex >= factor {
        subindex -= factor;
        index += 1;
    }
    (index, subindex)
}

#[cfg(test)]
mod tests {
    use crate::blackman_harris;
    use crate::get_nearest_time;
    use crate::get_nearest_times_2;
    use crate::get_nearest_times_4;
    use crate::interp_cubic;
    use crate::interp_lin;
    use crate::make_sincs;
    use crate::Interpolation;
    use crate::Resampler;
    use crate::{SincFixedIn, SincFixedOut};

    #[test]
    fn sincs() {
        let sincs = make_sincs::<f64>(16, 4, 1.0);
        println!("{:?}", sincs);
        assert_eq!(sincs[3][8], 1.0);
    }

    #[test]
    fn blackman() {
        let wnd = blackman_harris::<f64>(16);
        assert_eq!(wnd[8], 1.0);
        assert!(wnd[0] < 0.001);
        assert!(wnd[15] < 0.01);
    }

    #[test]
    fn int_cubic() {
        let yvals = vec![0.0f64, 2.0f64, 4.0f64, 6.0f64];
        let interp = interp_cubic(0.5f64, &yvals);
        assert_eq!(interp, 3.0f64);
    }

    #[test]
    fn int_lin() {
        let yvals = vec![1.0f64, 5.0f64];
        let interp = interp_lin(0.25f64, &yvals);
        assert_eq!(interp, 2.0f64);
    }

    #[test]
    fn get_nearest_2() {
        let t = 5.9f64;
        let mut times = vec![(0isize, 0isize); 2];
        get_nearest_times_2(t, 8, &mut times);
        assert_eq!(times[0], (5, 7));
        assert_eq!(times[1], (6, 0));
    }

    #[test]
    fn get_nearest_4() {
        let t = 5.9f64;
        let mut times = vec![(0isize, 0isize); 4];
        get_nearest_times_4(t, 8, &mut times);
        assert_eq!(times[0], (5, 6));
        assert_eq!(times[1], (5, 7));
        assert_eq!(times[2], (6, 0));
        assert_eq!(times[3], (6, 1));
    }

    #[test]
    fn get_nearest_4_neg() {
        let t = -5.999f64;
        let mut times = vec![(0isize, 0isize); 4];
        get_nearest_times_4(t, 8, &mut times);
        assert_eq!(times[0], (-7, 7));
        assert_eq!(times[1], (-6, 0));
        assert_eq!(times[2], (-6, 1));
        assert_eq!(times[3], (-6, 2));
    }

    #[test]
    fn get_nearest_4_zero() {
        let t = -0.00001f64;
        let mut times = vec![(0isize, 0isize); 4];
        get_nearest_times_4(t, 8, &mut times);
        assert_eq!(times[0], (-1, 6));
        assert_eq!(times[1], (-1, 7));
        assert_eq!(times[2], (0, 0));
        assert_eq!(times[3], (0, 1));
    }

    #[test]
    fn get_nearest_single() {
        let t = 5.5f64;
        let time = get_nearest_time(t, 8);
        assert_eq!(time, (5, 4));
    }

    #[test]
    fn make_resampler_fi() {
        let mut resampler =
            SincFixedIn::<f64>::new(1.2, 64, 0.95, 16, Interpolation::Cubic, 1024, 2);
        let waves = vec![vec![0.0f64; 1024]; 2];
        let out = resampler.process(&waves).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].len() > 1150 && out[0].len() < 1250);
    }

    #[test]
    fn make_resampler_fo() {
        let mut resampler =
            SincFixedOut::<f64>::new(1.2, 64, 0.95, 16, Interpolation::Cubic, 1024, 2);
        let frames = resampler.nbr_frames_needed();
        println!("{}", frames);
        assert!(frames > 800 && frames < 900);
        let waves = vec![vec![0.0f64; frames]; 2];
        let out = resampler.process(&waves).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), 1024);
    }
}
