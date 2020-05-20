

//! The PIZ compression method is a wavelet compression,
//! based on the PIZ image format, customized for OpenEXR.
// inspired by  https://github.com/AcademySoftwareFoundation/openexr/blob/master/OpenEXR/IlmImf/ImfPizCompressor.cpp

mod huffman;
mod wavelet;

use super::*;
use super::Result;
use crate::meta::attributes::{IntRect, SampleType, ChannelList};
use crate::io::Data;
use crate::math::Vec2;


const U16_RANGE: usize = (1_i32 << 16_i32) as usize;
const BITMAP_SIZE: usize  = (U16_RANGE as i32 >> 3_i32) as usize;

#[derive(Eq, PartialEq, Ord, PartialOrd, Debug, Copy, Clone)]
enum Format {
    Independent,
    Native
}

#[derive(Debug)]
struct ChannelData {
    tmp_start_index: usize,
    tmp_end_index: usize,

    resolution: Vec2<usize>,
    y_sampling: usize,
    samples_per_pixel: usize,
}


pub fn decompress_bytes(
    channels: &ChannelList,
    compressed: ByteVec,
    rectangle: IntRect,
    expected_byte_size: usize,
) -> Result<ByteVec>
{
    if compressed.is_empty() {
        return Ok(Vec::new())
    }

    let mut tmp_buffer = vec![0_u16; expected_byte_size / 2]; // TODO create inside huffman::decompress?

    let mut channel_data: Vec<ChannelData> = Vec::with_capacity(channels.list.len());
    let mut tmp_read_index = 0;

    for channel in channels.list.iter() {
        let channel = ChannelData {
            tmp_start_index: tmp_read_index,
            tmp_end_index: tmp_read_index,
            y_sampling: channel.sampling.y(),
            resolution: channel.subsampled_resolution(rectangle.size),
            samples_per_pixel: channel.sample_type.bytes_per_sample() / SampleType::F16.bytes_per_sample()
        };

        inspect!(channel);

        tmp_read_index += channel.resolution.area() * channel.samples_per_pixel;
        channel_data.push(channel);
    }

    debug_assert_eq!(tmp_read_index, tmp_buffer.len());
    inspect!(channel_data);

    let mut bitmap = vec![0_u8; BITMAP_SIZE]; // FIXME use bit_vec!

    let mut remaining_input = compressed.as_slice();
    let min_non_zero = u16::read(&mut remaining_input).unwrap() as usize;
    let max_non_zero = u16::read(&mut remaining_input).unwrap() as usize;
    inspect!(min_non_zero, max_non_zero);

    if max_non_zero >= BITMAP_SIZE {
        println!("invalid bitmap size");
        return Err(Error::invalid("compression data"));
    }

    if min_non_zero <= max_non_zero {
        u8::read_slice(&mut remaining_input, &mut bitmap[min_non_zero ..= max_non_zero]).unwrap();
    }

    let (lookup_table, max_value) = reverse_lookup_table_from_bitmap(&bitmap);
    inspect!(max_value);

    let length = i32::read(&mut remaining_input).unwrap();
    inspect!(length);

    if length < 0 || length as usize > remaining_input.len() {
        println!("invalid array length");
        return Err(Error::invalid("compression data"));
    }

    inspect!(length, remaining_input.len(), &remaining_input[..20]);
    huffman::decompress(&remaining_input[..length as usize], &mut tmp_buffer).unwrap();


    for channel in &channel_data {
        let u16_count = channel.resolution.area() * channel.samples_per_pixel;
        let u16s = &mut tmp_buffer[channel.tmp_start_index .. channel.tmp_start_index + u16_count];

        for offset in 0..channel.samples_per_pixel { // if channel is 32 bit, compress interleaved as two 16 bit values
            inspect!(channel);

            wavelet::decode(
                &mut u16s[offset..],
                channel.resolution,
                Vec2(channel.samples_per_pixel, channel.resolution.x() * channel.samples_per_pixel),
                max_value
            )?;
        }
    }

//        // Expand the pixel data to their original range
    apply_lookup_table(&mut tmp_buffer, &lookup_table);

    let has_only_half_channels = channels.list
        .iter().all(|channel| channel.sample_type == SampleType::F16);

    // We can support uncompressed data in the machine's native format
    // if all image channels are of type HALF, and if the Xdr and the
    // native representations of a half have the same size.
    let format = {
        if has_only_half_channels { Format::Native }
        else { Format::Independent } // half is always 16 bit in Rust
    };


    // let out_buffer_size = (max_scan_line_size * scan_line_count) + 65536 + 8192; // TODO not use expected byte size?
    let mut out = Vec::with_capacity(expected_byte_size);

    for y in rectangle.position.y() .. rectangle.end().y() {
        for channel in &mut channel_data {
            if mod_p(y, channel.y_sampling as i32) != 0 {
                continue;
            }

            let u16s_per_line = channel.resolution.x() * channel.samples_per_pixel;

            // if format == Format::Independent {
            let next_tmp_end_index = channel.tmp_end_index + u16s_per_line;
            let values = &tmp_buffer[channel.tmp_end_index .. next_tmp_end_index];

            if format == Format::Independent {
                u16::write_slice(&mut out, values).expect("write to in-memory failed");
            }
            else { // machine-dependent data format is a simple memcpy
                use lebe::io::WriteEndian;
                out.write_as_native_endian(values).expect("write to in-memory failed");
            }

            channel.tmp_end_index = next_tmp_end_index;
        }
    }

    for index in 1..channel_data.len() {
        debug_assert_eq!(channel_data[index - 1].tmp_end_index, channel_data[index].tmp_start_index);
    }

    debug_assert_eq!(channel_data.last().unwrap().tmp_end_index, tmp_buffer.len());
    debug_assert_eq!(out.len(), expected_byte_size);

    Ok(out)
}



pub fn compress_bytes(
    channels: &ChannelList,
    bytes: Bytes<'_>,
    rectangle: IntRect
) -> Result<ByteVec>
{
    if bytes.is_empty() {
        return Ok(Vec::new());
    }

    let mut tmp = vec![ 0_u16; bytes.len() / 2 ];
    let mut channel_data = Vec::new();

    let mut tmp_end_index = 0;
    for channel in &channels.list {
        let number_samples = channel.subsampled_resolution(rectangle.size);
        let byte_size = channel.sample_type.bytes_per_sample() / SampleType::F16.bytes_per_sample();
        let byte_count = byte_size * number_samples.area();

        let channel = ChannelData {
            tmp_end_index,
            tmp_start_index: tmp_end_index,
            y_sampling: channel.sampling.y(),
            resolution: number_samples,
            samples_per_pixel: byte_size,
        };

        tmp_end_index += byte_count;
        channel_data.push(channel);
    }

    debug_assert_eq!(tmp_end_index, tmp.len());

    let has_only_half_channels = channels.list
        .iter().all(|channel| channel.sample_type == SampleType::F16);

    // We can support uncompressed data in the machine's native format
    // if all image channels are of type HALF, and if the Xdr and the
    // native representations of a half have the same size.
    let format = {
        if has_only_half_channels { Format::Native }
        else { Format::Independent } // half is always 16 bit in Rust
    };

    let mut byte_read = bytes;
    for y in rectangle.position.y() .. rectangle.end().y() {
        for channel in &mut channel_data {
            if mod_p(y, channel.y_sampling as i32) != 0 { continue; }
            let u16s_per_line = channel.resolution.x() * channel.samples_per_pixel;
            let next_tmp_end_index = channel.tmp_end_index + u16s_per_line;
            let mut target = &mut tmp[channel.tmp_end_index ..next_tmp_end_index];

            if format == Format::Independent {
                u16::read_slice(&mut byte_read, target).expect("in-memory read failed");
            }
            else {
                use lebe::io::ReadEndian;
                byte_read.read_from_native_endian_into(target).expect("in-memory read failed");
            }

            channel.tmp_end_index = next_tmp_end_index;
        }
    }


    let (min_non_zero, max_non_zero, bitmap) = bitmap_from_data(&tmp);
    let (max_value, table) = forward_lookup_table_from_bitmap(&bitmap);
    apply_lookup_table(&mut tmp, &table);

    let mut output = Vec::with_capacity(bytes.len() / 3);
    (min_non_zero as u16).write(&mut output)?;
    (max_non_zero as u16).write(&mut output)?;

    if min_non_zero <= max_non_zero {
        output.extend_from_slice(&bitmap[min_non_zero ..= max_non_zero]);
    }

    for channel in channel_data {
        for offset in 0 .. channel.samples_per_pixel {
            wavelet::encode(
                &mut tmp[channel.tmp_start_index + offset .. channel.tmp_end_index],
                channel.resolution,
                Vec2(channel.samples_per_pixel, channel.resolution.x() * channel.samples_per_pixel),
                max_value
            )?;
        }
    }

    let compressed: Vec<u8> = huffman::compress(&tmp)?;
    (compressed.len() as i32).write(&mut output).expect("in-memory write failed");
    output.extend_from_slice(&compressed);

    Ok(output)
}

//
// Integer division and remainder where the
// remainder of x/y is always positive:
//
//	divp(x,y) == floor (double(x) / double (y))
//	modp(x,y) == x - y * divp(x,y)
//
//
//    inline int
//    divp (int x, int y)
//    {
//       return (x >= 0)? ((y >= 0)?  (     x  / y): -(      x  / -y)):
//       ((y >= 0)? -((y-1-x) / y):  ((-y-1-x) / -y));
//    }
//
//
//    inline int
//    modp (int x, int y)
//    {
//       return x - y * divp (x, y);
//    }

fn div_p (x: i32, y: i32) -> i32 {
    if x >= 0 {
        if y >= 0 { x  / y }
        else { -(x  / -y) }
    }
    else {
        if y >= 0 { -((y-1-x) / y) }
        else { (-y-1-x) / -y }
    }
}

fn mod_p(x: i32, y: i32) -> i32 {
    x - y * div_p(x, y)
}




fn reverse_lookup_table_from_bitmap(bitmap: Bytes<'_>) -> (Vec<u16>, u16) {
//    int k = 0;
//
//    for (int i = 0; i < USHORT_RANGE; ++i)
//    {
//        if ((i == 0) || (bitmap[i >> 3] & (1 << (i & 7))))
//        lut[k++] = i;
//    }
//
//    int n = k - 1;
//
//    while (k < USHORT_RANGE)
//    lut[k++] = 0;
//
//    return n;		// maximum k where lut[k] is non-zero,


    let mut table = Vec::with_capacity(U16_RANGE);

    for index in 0 .. U16_RANGE {
        if index == 0 || ((bitmap[index >> 3] as usize & (1 << (index & 7))) != 0) {
            table.push(index as u16);
        }
    }

    let max_value = (table.len() - 1) as u16;

    // fill remaining up to u16 range
    assert!(table.len() <= U16_RANGE);
    table.resize(U16_RANGE, 0);

    (table, max_value)
}

fn apply_lookup_table(data: &mut [u16], table: &[u16]) {
    for data in data {
        *data = table[*data as usize];
    }
}


pub fn bitmap_from_data(data: &[u16]) -> (usize, usize, [u8; BITMAP_SIZE]) {
    let mut bitmap = [0_u8; BITMAP_SIZE];

    for value in data {
        bitmap[*value as usize >> 3] |= 1 << (*value as u8 & 7);
    }

    bitmap[0] = bitmap[0] & !1; // zero is not explicitly stored in the bitmap; we assume that the data always contain zeroes

    let mut min = bitmap.len() - 1;
    let mut max = 0;

    for (bit_index, &bit) in bitmap.iter().enumerate() { // TODO do not go through bitmap unconditionally!
        if bit != 0 {
            min = min.min(bit_index);
            max = max.max(bit_index);
        }
    }

    (min, max, bitmap)
}

pub fn forward_lookup_table_from_bitmap(bitmap: &[u8]) -> (u16, [u16; U16_RANGE]) {
    debug_assert_eq!(bitmap.len(), BITMAP_SIZE);

    let mut table = [0_u16; U16_RANGE];
    let mut count = 0_usize;

    for (index, entry) in table.iter_mut().enumerate() {
        if index == 0 || bitmap[index >> 3] as usize & (1 << (index & 7)) != 0 {
            *entry = count as u16;
            count += 1;
        }
    }

    ((count - 1) as u16, table)
}

#[cfg(test)]
mod test {
    use crate::prelude::*;
    use crate::meta::*;
    use crate::meta::attributes::*;
    use crate::compression::ByteVec;
    use crate::compression::piz;

    fn test_roundtrip_noise_with(channels: ChannelList, rectangle: IntRect){
        let pixel_bytes: ByteVec = (0 .. channels.bytes_per_pixel * rectangle.size.area())
            .map(|_| rand::random()).collect();

        let compressed = piz::compress_bytes(&channels, &pixel_bytes, rectangle).unwrap();
        let decompressed = piz::decompress_bytes(&channels, compressed, rectangle, pixel_bytes.len()).unwrap();

        assert_eq!(pixel_bytes, decompressed);
    }


    #[test]
    fn roundtrip_any_sample_type(){
        for &sample_type in &[SampleType::F16, SampleType::F32, SampleType::U32] {
            let channel = Channel {
                sample_type,

                name: Default::default(),
                quantize_linearly: false,
                sampling: Vec2(1,1)
            };

            let channels = ChannelList::new(smallvec![ channel.clone(), channel ]);

            let rectangle = IntRect {
                position: Vec2(-30, 100),
                size: Vec2(322, 731),
            };

            test_roundtrip_noise_with(channels, rectangle);
        }
    }

    #[test]
    fn roundtrip_two_channels(){
        let channel = Channel {
            sample_type: SampleType::F16,

            name: Default::default(),
            quantize_linearly: false,
            sampling: Vec2(1,1)
        };

        let channel2 = Channel {
            sample_type: SampleType::F32,

            name: Default::default(),
            quantize_linearly: false,
            sampling: Vec2(1,1)
        };

        let channels = ChannelList::new(smallvec![ channel, channel2 ]);

        let rectangle = IntRect {
            position: Vec2(-3, 1),
            size: Vec2(2323, 3132),
        };

        test_roundtrip_noise_with(channels, rectangle);
    }



    #[test]
    fn roundtrip_seven_channels(){
        let channels = ChannelList::new(smallvec![
            Channel {
                sample_type: SampleType::F32,

                name: Default::default(),
                quantize_linearly: false,
                sampling: Vec2(1,1)
            },

            Channel {
                sample_type: SampleType::F32,

                name: Default::default(),
                quantize_linearly: false,
                sampling: Vec2(1,1)
            },

            Channel {
                sample_type: SampleType::F32,

                name: Default::default(),
                quantize_linearly: false,
                sampling: Vec2(1,1)
            },

            Channel {
                sample_type: SampleType::F16,

                name: Default::default(),
                quantize_linearly: false,
                sampling: Vec2(1,1)
            },

            Channel {
                sample_type: SampleType::F32,

                name: Default::default(),
                quantize_linearly: false,
                sampling: Vec2(1,1)
            },

            Channel {
                sample_type: SampleType::F32,

                name: Default::default(),
                quantize_linearly: false,
                sampling: Vec2(1,1)
            },

            Channel {
                sample_type: SampleType::U32,

                name: Default::default(),
                quantize_linearly: false,
                sampling: Vec2(1,1)
            },
        ]);

        let rectangle = IntRect {
            position: Vec2(-3, 1),
            size: Vec2(2323, 3132),
        };

        test_roundtrip_noise_with(channels, rectangle);
    }

}