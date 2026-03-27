#!/usr/bin/env python3
"""
Inspect Matroska/WebM files to show Cluster sizes, block counts, and keyframes.
Reports per-track block and keyframe counts, distinguishing video from audio.
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

def read_uint(data):
    return int.from_bytes(data, byteorder='big')

def read_utf8(data):
    return data.decode('utf-8', errors='replace').rstrip('\x00')

# EBML element IDs
EBML_ID = {
    0x1A45DFA3: 'EBML',
    0x18538067: 'Segment',
    0x1549A966: 'Info',
    0x1654AE6B: 'Tracks',
    0xAE:       'TrackEntry',
    0xD7:       'TrackNumber',
    0x83:       'TrackType',
    0x86:       'CodecID',
    0x1F43B675: 'Cluster',
    0xE7:       'Timecode',
    0xA3:       'SimpleBlock',
    0xA0:       'BlockGroup',
    0xA1:       'Block',
}

# TrackType values
TRACK_TYPE_VIDEO = 1
TRACK_TYPE_AUDIO = 2
TRACK_TYPE_SUBTITLE = 17

def parse_tracks(f, size):
    """Parse the Tracks element and return a dict: track_number -> track_type"""
    tracks = {}
    end = f.tell() + size
    while f.tell() < end:
        eid = read_id(f)
        if eid is None: break
        vint = read_vint(f)
        if vint is None: break
        _, elem_size, is_unknown = vint
        if is_unknown:
            break

        name = EBML_ID.get(eid, '')
        if name == 'TrackEntry':
            track_num = None
            track_type = None
            codec_id = None
            te_end = f.tell() + elem_size
            while f.tell() < te_end:
                te_eid = read_id(f)
                if te_eid is None: break
                te_vint = read_vint(f)
                if te_vint is None: break
                _, te_size, te_unknown = te_vint
                if te_unknown:
                    break
                te_name = EBML_ID.get(te_eid, '')
                data = f.read(te_size)
                if te_name == 'TrackNumber':
                    track_num = read_uint(data)
                elif te_name == 'TrackType':
                    track_type = read_uint(data)
                elif te_name == 'CodecID':
                    codec_id = read_utf8(data)
            if track_num is not None and track_type is not None:
                tracks[track_num] = {'type': track_type, 'codec': codec_id or ''}
        else:
            f.seek(elem_size, 1)
    return tracks

def track_type_name(t):
    if t == TRACK_TYPE_VIDEO: return 'video'
    if t == TRACK_TYPE_AUDIO: return 'audio'
    if t == TRACK_TYPE_SUBTITLE: return 'subtitle'
    return f'type{t}'

def analyze_mkv(filepath):
    print(f"Analyzing {filepath}...")
    try:
        with open(filepath, 'rb') as f:
            track_info = {}  # track_num -> {type, codec}

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

                if name == 'Tracks':
                    track_info = parse_tracks(f, size)
                    print(f"\nTracks found:")
                    for tn, ti in sorted(track_info.items()):
                        print(f"  Track {tn}: {track_type_name(ti['type'])} ({ti['codec']})")
                    continue
                
                if name == 'Cluster':
                    cluster_size = size
                    print(f"\n[Cluster] pos: {pos}, size: {'Unknown' if is_unknown else cluster_size} bytes")
                    
                    if is_unknown:
                        print("  (Cannot skip unknown cluster, script needs full parser for streaming mode)")
                        continue
                        
                    cluster_end = f.tell() + size
                    
                    # per-track counters: track_num -> {blocks, keyframes}
                    per_track = {}
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
                            track_vint = read_vint(f)
                            if track_vint is None:
                                f.seek(c_size, 1)
                                continue
                            track_num = track_vint[1]
                            # 2 bytes relative timecode
                            f.read(2)
                            # flags byte
                            flags_data = f.read(1)
                            if not flags_data:
                                break
                            flags = flags_data[0]
                            is_keyframe = bool(flags & 0x80)

                            if track_num not in per_track:
                                per_track[track_num] = {'blocks': 0, 'keyframes': 0}
                            per_track[track_num]['blocks'] += 1
                            if is_keyframe:
                                per_track[track_num]['keyframes'] += 1

                            remaining = c_size - (track_vint[0] + 3)
                            f.seek(remaining, 1)
                        elif c_name == 'BlockGroup':
                            # Read inside BlockGroup to find the Block element
                            bg_end = f.tell() + c_size
                            found_block = False
                            while f.tell() < bg_end:
                                bg_eid = read_id(f)
                                if bg_eid is None: break
                                bg_vint = read_vint(f)
                                if bg_vint is None: break
                                _, bg_size, _ = bg_vint
                                bg_name = EBML_ID.get(bg_eid, '')
                                if bg_name == 'Block':
                                    track_vint = read_vint(f)
                                    if track_vint is None:
                                        f.seek(bg_size, 1)
                                        break
                                    track_num = track_vint[1]
                                    # 2 bytes relative timecode
                                    f.read(2)
                                    # flags byte (Block flags: bit 7 is NOT keyframe for Block inside BlockGroup;
                                    # keyframe is implied when no ReferenceBlock child exists)
                                    f.read(1)
                                    remaining = bg_size - (track_vint[0] + 3)
                                    f.seek(remaining, 1)
                                    if track_num not in per_track:
                                        per_track[track_num] = {'blocks': 0, 'keyframes': 0}
                                    per_track[track_num]['blocks'] += 1
                                    # For BlockGroup, keyframe = no ReferenceBlock child
                                    # We'll mark it as unknown here and handle below
                                    found_block = True
                                    break
                                else:
                                    f.seek(bg_size, 1)
                            if not found_block:
                                f.seek(bg_end - f.tell(), 1)
                            else:
                                # skip rest of BlockGroup
                                remaining = bg_end - f.tell()
                                if remaining > 0:
                                    f.seek(remaining, 1)
                        else:
                            f.seek(c_size, 1)

                    # Print per-track summary
                    total_blocks = sum(v['blocks'] for v in per_track.values())
                    total_keyframes = sum(v['keyframes'] for v in per_track.values())
                    print(f"  -> Total Blocks: {total_blocks}, Total Keyframes (SimpleBlock): {total_keyframes}")
                    for tn in sorted(per_track.keys()):
                        tc = per_track[tn]
                        ti = track_info.get(tn, {})
                        ttype = track_type_name(ti.get('type', 0)) if ti else f'track{tn}'
                        codec = ti.get('codec', '') if ti else ''
                        print(f"     Track {tn} ({ttype} {codec}): {tc['blocks']} blocks, {tc['keyframes']} keyframes")
                    
                else:
                    if is_unknown:
                        print(f"Unknown size for {name} at {pos}, stopping")
                        break
                    f.seek(size, 1)
    except Exception as e:
        import traceback
        traceback.print_exc()
        print(f"Error: {e}")

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python cluster_inspector.py <file.mkv>")
        sys.exit(1)
    analyze_mkv(sys.argv[1])
