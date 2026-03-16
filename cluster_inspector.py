#!/usr/bin/env python3
"""
Inspect Matroska/WebM files to show Cluster sizes, block counts, and keyframes.
"""
import sys

def read_vint(f):
    first_byte_data = f.read(1)
    if not first_byte_data: return None
    first_byte = first_byte_data[0]
    if first_byte == 0: return None
    
    mask = 0x80
    length = 1
    while length <= 8 and (first_byte & mask) == 0:
        mask >>= 1
        length += 1
        
    value = first_byte & (mask - 1)
    if length > 1:
        rest = f.read(length - 1)
        if len(rest) < length - 1: return None
        for b in rest:
            value = (value << 8) | b
            
    is_unknown = False
    if value == ((1 << (7 * length)) - 1):
        is_unknown = True
        
    return length, value, is_unknown

def read_id(f):
    first_byte_data = f.read(1)
    if not first_byte_data: return None
    first_byte = first_byte_data[0]
    
    mask = 0x80
    length = 1
    while length <= 4 and (first_byte & mask) == 0:
        mask >>= 1
        length += 1
        
    value = first_byte
    if length > 1:
        rest = f.read(length - 1)
        if len(rest) < length - 1: return None
        for b in rest:
            value = (value << 8) | b
    return value

EBML_ID = {
    0x1F43B675: 'Cluster',
    0xE7: 'Timecode',
    0xA3: 'SimpleBlock',
    0xA0: 'BlockGroup',
    0xA1: 'Block',
    0x18538067: 'Segment'
}

def analyze_mkv(filepath):
    print(f"Analyzing {filepath}...")
    try:
        with open(filepath, 'rb') as f:
            while True:
                pos = f.tell()
                eid = read_id(f)
                if not eid: break
                
                name = EBML_ID.get(eid, f"Unknown (0x{eid:X})")
                vint = read_vint(f)
                if not vint: break
                size_len, size, is_unknown = vint
                
                if name == 'Segment':
                    print(f"Segment found at {pos}, size: {'Unknown' if is_unknown else size}")
                    continue
                
                if name == 'Cluster':
                    cluster_size = size
                    print(f"\n[Cluster] pos: {pos}, size: {'Unknown' if is_unknown else cluster_size} bytes")
                    
                    if is_unknown:
                        print("  (Cannot skip unknown cluster, script needs full parser for streaming mode)")
                        # In streaming, we just keep reading inside the cluster
                        continue
                        
                    cluster_end = f.tell() + size
                    
                    block_count = 0
                    keyframe_count = 0
                    first_timecode = -1
                    
                    while f.tell() < cluster_end:
                        c_pos = f.tell()
                        c_eid = read_id(f)
                        if not c_eid: break
                        
                        c_vint = read_vint(f)
                        if not c_vint: break
                        c_size_len, c_size, _ = c_vint
                        
                        c_name = EBML_ID.get(c_eid, "")
                        
                        if c_name == 'Timecode':
                            tc_data = f.read(c_size)
                            tc = int.from_bytes(tc_data, byteorder='big')
                            print(f"  Cluster Timecode: {tc}")
                            first_timecode = tc
                        elif c_name == 'SimpleBlock':
                            block_count += 1
                            track_vint = read_vint(f)
                            # timecode relative
                            f.read(2) 
                            # flags
                            flags = f.read(1)[0]
                            is_keyframe = bool(flags & 0x80)
                            if is_keyframe:
                                keyframe_count += 1
                            
                            remaining = c_size - (track_vint[0] + 3)
                            f.seek(remaining, 1)
                        elif c_name == 'BlockGroup':
                            block_count += 1
                            # Simplified: just skip it, finding keyframes in BlockGroup needs reading child Block
                            f.seek(c_size, 1)
                        else:
                            f.seek(c_size, 1)
                            
                    print(f"  -> Total Blocks: {block_count}, Keyframes: {keyframe_count}")
                    
                else:
                    if is_unknown:
                        print(f"Unknown size for {name} at {pos}, stopping")
                        break
                    f.seek(size, 1)
    except Exception as e:
        print(f"Error: {e}")

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python cluster_inspector.py <file.mkv>")
        sys.exit(1)
    analyze_mkv(sys.argv[1])
