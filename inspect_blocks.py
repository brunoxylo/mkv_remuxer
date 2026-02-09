#!/usr/bin/env python3
"""
Inspect Matroska SimpleBlock flags to see invisible/keyframe/discardable bits
"""
import sys
import struct

def vint_read(data, pos):
    """Read a variable-length integer"""
    if pos >= len(data):
        return None, pos
    
    first_byte = data[pos]
    if first_byte == 0:
        return None, pos
    
    # Count leading zeros to determine length
    mask = 0x80
    length = 1
    while length <= 8 and (first_byte & mask) == 0:
        mask >>= 1
        length += 1
    
    if length > 8:
        return None, pos
    
    # Read the value
    value = first_byte & (mask - 1)
    for i in range(1, length):
        if pos + i >= len(data):
            return None, pos
        value = (value << 8) | data[pos + i]
    
    return value, pos + length

def parse_simpleblock(block_data):
    """Parse a SimpleBlock and return its details"""
    if len(block_data) < 4:
        return None
    
    # Read track number (vint)
    track_num, pos = vint_read(block_data, 0)
    if track_num is None:
        return None
    
    # Read timestamp (2 bytes, signed big-endian)
    timestamp = struct.unpack('>h', block_data[pos:pos+2])[0]
    pos += 2
    
    # Flags byte
    flags = block_data[pos]
    
    keyframe = bool(flags & 0x80)
    invisible = bool(flags & 0x10)
    lacing = (flags >> 1) & 0x03
    discardable = bool(flags & 0x01)
    
    return {
        'track': track_num,
        'timestamp': timestamp,
        'flags': flags,
        'keyframe': keyframe,
        'invisible': invisible,
        'lacing': lacing,
        'discardable': discardable,
    }

def read_element_id_size(f):
    """Read element ID and size"""
    # Read ID (vint)
    first = f.read(1)
    if not first:
        return None, None
    
    first_byte = first[0]
    mask = 0x80
    id_len = 1
    while id_len <= 4 and (first_byte & mask) == 0:
        mask >>= 1
        id_len += 1
    
    element_id = int.from_bytes(first + f.read(id_len - 1), 'big')
    
    # Read size (vint)
    first = f.read(1)
    if not first:
        return element_id, None
    
    first_byte = first[0]
    mask = 0x80
    size_len = 1
    while size_len <= 8 and (first_byte & mask) == 0:
        mask >>= 1
        size_len += 1
    
    size_bytes = first + f.read(size_len - 1)
    # Check for unknown size
    if size_bytes == bytes([0xFF] * size_len):
        return element_id, -1  # Unknown size
    
    size_value = int.from_bytes(size_bytes, 'big') & ((1 << (7 * size_len)) - 1)
    
    return element_id, size_value

def inspect_webm(filename, max_blocks=50):
    """Inspect a WebM/MKV file and print SimpleBlock information"""
    CLUSTER_ID = 0x1F43B675
    SIMPLEBLOCK_ID = 0xA3
    
    with open(filename, 'rb') as f:
        block_count = 0
        current_cluster_time = 0
        
        while block_count < max_blocks:
            element_id, element_size = read_element_id_size(f)
            
            if element_id is None:
                break
                
            if element_id == CLUSTER_ID:
                # In a cluster - scan for timestamp and blocks
                cluster_start = f.tell()
                if element_size == -1:
                    # Unknown size, read until next cluster
                    element_size = 1024 * 1024  # Read up to 1MB
                
                cluster_end = cluster_start + element_size if element_size > 0 else f.seek(0, 2)
                f.seek(cluster_start)
                
                # Read cluster contents
                while f.tell() < cluster_end:
                    sub_id, sub_size = read_element_id_size(f)
                    if sub_id is None:
                        break
                    
                    if sub_id == 0xE7:  # Timestamp
                        current_cluster_time = int.from_bytes(f.read(sub_size), 'big')
                    elif sub_id == SIMPLEBLOCK_ID:
                        block_data = f.read(sub_size)
                        info = parse_simpleblock(block_data)
                        if info:
                            abs_time_ms = (current_cluster_time + info['timestamp'])
                            track_type = "video" if info['track'] == 1 else f"audio/{info['track']}"
                            
                            flags_str = f"0x{info['flags']:02x}"
                            markers = []
                            if info['keyframe']:
                                markers.append("KEY")
                            if info['invisible']:
                                markers.append("INVISIBLE")
                            if info['discardable']:
                                markers.append("DISC")
                            
                            print(f"[{track_type:8s}] {abs_time_ms:6d}ms | "
                                  f"Flags: {flags_str} | {' '.join(markers) if markers else 'none'}")
                            
                            if info['track'] == 1:  # Only count video blocks
                                block_count += 1
                                if block_count >= max_blocks:
                                    return
                    else:
                        # Skip other elements
                        if sub_size > 0 and sub_size < 1024*1024:
                            f.read(sub_size)
                        elif sub_id == 0xA0:  # BlockGroup
                            # Skip to next
                            pass
                        else:
                            break
            else:
                # Skip this element
                if element_size > 0 and element_size < 100*1024*1024:
                    f.seek(element_size, 1)
                else:
                    # Try to find next cluster
                    chunk = f.read(4096)
                    if not chunk:
                        break

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python3 inspect_blocks.py <webm_file> [max_blocks]")
        sys.exit(1)
    
    filename = sys.argv[1]
    max_blocks = int(sys.argv[2]) if len(sys.argv) > 2 else 50
    
    inspect_webm(filename, max_blocks)
