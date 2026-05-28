use crate::Result;
use log::debug;
use mkv_element::io::blocking_impl::WriteTo;
use mkv_element::prelude::Tracks;

fn read_vint_id(data: &[u8]) -> Option<(u64, usize)> {
    if data.is_empty() {
        return None;
    }
    let first = data[0];
    let mut mask = 0x80;
    let mut len = 1;
    while mask > 0 && (first & mask) == 0 {
        mask >>= 1;
        len += 1;
    }
    if mask == 0 || data.len() < len {
        return None;
    }
    let mut val = 0;
    for i in 0..len {
        val = (val << 8) | (data[i] as u64);
    }
    Some((val, len))
}

fn read_vint_size(data: &[u8]) -> Option<(u64, usize)> {
    if data.is_empty() {
        return None;
    }
    let first = data[0];
    let mut mask = 0x80;
    let mut len = 1;
    while mask > 0 && (first & mask) == 0 {
        mask >>= 1;
        len += 1;
    }
    if mask == 0 || data.len() < len {
        return None;
    }
    let mut val = (first & !mask) as u64;
    for i in 1..len {
        val = (val << 8) | (data[i] as u64);
    }
    Some((val, len))
}

fn is_container(id: u64) -> bool {
    matches!(
        id,
        0x1654AE6B | // Tracks
        0xAE |       // TrackEntry
        0xE0 |       // Video
        0xE1 |       // Audio
        0x55B0 |     // Colour
        0x55D0 |     // MasteringMetadata
        0x6D80 |     // ContentEncodings
        0x6240 |     // ContentEncoding
        0x41E4 |     // BlockAdditionMapping
        0x6624 // TrackTranslate
    )
}

pub fn patch_tracks_for_webm(tracks: &Tracks) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    tracks
        .write_to(&mut data)
        .map_err(|e| crate::Error::InvalidConfig(format!("Failed to serialize tracks: {}", e)))?;

    let mut offset = 0;
    let mut voided_count = 0;

    while offset < data.len() {
        let Some((id, id_len)) = read_vint_id(&data[offset..]) else {
            break;
        };
        let Some((size, size_len)) = read_vint_size(&data[offset + id_len..]) else {
            break;
        };

        let header_len = id_len + size_len;
        let element_len = header_len + size as usize;

        if id == 0x9D {
            //|| id == 0x52F1 { // FieldOrder or AudioEmphasis
            // Replace with Void (0xEC)
            let l = element_len;
            if l >= 2 {
                data[offset] = 0xEC; // Void ID
                if l - 2 <= 127 {
                    data[offset + 1] = 0x80 | ((l - 2) as u8);
                    for i in 2..l {
                        data[offset + i] = 0;
                    }
                } else if l - 3 <= 16383 {
                    data[offset + 1] = 0x40 | (((l - 3) >> 8) as u8);
                    data[offset + 2] = ((l - 3) & 0xFF) as u8;
                    for i in 3..l {
                        data[offset + i] = 0;
                    }
                } else {
                    debug!(
                        "Element too large to void easily: ID 0x{:X}, length {}",
                        id, l
                    );
                }
            }
            debug!("Voided non-WebM Track element: 0x{:X}", id);
            voided_count += 1;
            offset += element_len;
        } else if is_container(id) {
            // Step into the container by advancing past its header
            offset += header_len;
        } else {
            // Skip the element payload
            offset += element_len;
        }
    }

    if voided_count > 0 {
        debug!("Voided {} non-WebM Tracks element(s)", voided_count);
    }

    Ok(data)
}
