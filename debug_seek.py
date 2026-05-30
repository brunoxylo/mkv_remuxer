#!/usr/bin/env python3
"""
Debug script to investigate the 'Cluster timestamp not found or invalid' error 
when seeking to 2640.79s in braven.webm.

This script:
1. Reads the timecode_scale from the file
2. Scans all clusters and reports their timestamps  
3. Specifically investigates the cluster that should contain 2640.79s
4. Checks for clusters with missing/malformed Timestamp (0xE7) child elements
"""
import sys
import struct

def read_vint(f):
    """Read an EBML variable-length integer. Returns (width, value, is_unknown) or None."""
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
    """Read an EBML element ID."""
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

def ebml_vint_width(first_byte):
    if first_byte == 0:
        return 8
    return first_byte.bit_length()
    # count leading zeros + 1
    width = 0
    mask = 0x80
    while mask and not (first_byte & mask):
        width += 1
        mask >>= 1
    return width + 1

def ebml_vint_width_rust(first_byte):
    """Match the Rust implementation: leading_zeros() + 1"""
    if first_byte == 0:
        return 8
    # Count leading zeros for a u8
    lz = 0
    for i in range(7, -1, -1):
        if first_byte & (1 << i):
            break
        lz += 1
    return lz + 1

def simulate_rust_read_cluster_timestamp_at(buf, timecode_scale):
    """
    Simulate the Rust read_cluster_timestamp_at function exactly.
    Returns (timestamp_ns, debug_info) or (None, debug_info).
    """
    debug = []
    n = len(buf)
    
    # Skip Cluster element ID (4 bytes)
    i = 4
    if i >= n:
        debug.append(f"Buffer too short after skipping 4-byte ID: len={n}")
        return None, debug
    
    # Skip Cluster size (variable-length VINT)
    size_width = ebml_vint_width_rust(buf[i])
    debug.append(f"Cluster size VINT starts at byte {i}, first_byte=0x{buf[i]:02X}, width={size_width}")
    i += size_width
    
    debug.append(f"After size VINT, scanning for Timestamp (0xE7) starting at byte {i}")
    
    # Scan up to 25 child elements looking for Timestamp (ID 0xE7)
    for attempt in range(25):
        if i >= n:
            debug.append(f"  Attempt {attempt}: reached end of buffer at byte {i}")
            break
        
        id_width = ebml_vint_width_rust(buf[i])
        debug.append(f"  Attempt {attempt}: byte {i}, raw=0x{buf[i]:02X}, id_width={id_width}")
        
        if i + id_width > n:
            debug.append(f"  Attempt {attempt}: not enough bytes for ID (need {id_width}, have {n-i})")
            break
        
        is_timestamp = id_width == 1 and buf[i] == 0xE7
        id_bytes = buf[i:i+id_width]
        debug.append(f"  Attempt {attempt}: ID bytes = {id_bytes.hex()}, is_timestamp={is_timestamp}")
        i += id_width
        
        if i >= n:
            debug.append(f"  Attempt {attempt}: reached end after ID")
            break
        
        data_size_width = ebml_vint_width_rust(buf[i])
        if i + data_size_width > n:
            debug.append(f"  Attempt {attempt}: not enough bytes for size VINT")
            break
        
        # Parse data size value (stripping marker bits, matching Rust's ebml_vint_value)
        width = data_size_width
        val = buf[i] & (0xFF >> width)
        for j in range(1, min(width, n - i)):
            val = (val << 8) | buf[i + j]
        data_size = val
        
        debug.append(f"  Attempt {attempt}: data_size_width={data_size_width}, data_size={data_size}")
        i += data_size_width
        
        if is_timestamp:
            if i + data_size > n:
                debug.append(f"  Attempt {attempt}: TIMESTAMP FOUND but data extends beyond buffer!")
                break
            ticks = 0
            for b in buf[i:i+data_size]:
                ticks = (ticks << 8) | b
            timestamp_ns = ticks * timecode_scale
            debug.append(f"  Attempt {attempt}: TIMESTAMP FOUND! ticks={ticks}, ns={timestamp_ns}, sec={timestamp_ns/1e9:.3f}")
            return timestamp_ns, debug
        
        i += data_size
    
    debug.append("FAILED: Timestamp element (0xE7) not found in buffer!")
    return None, debug


def main():
    filepath = sys.argv[1] if len(sys.argv) > 1 else "braven.webm"
    target_sec = float(sys.argv[2]) if len(sys.argv) > 2 else 2640.79
    
    print(f"=== Debugging seek to {target_sec}s in {filepath} ===\n")
    
    with open(filepath, 'rb') as f:
        import os
        file_size = os.path.getsize(filepath)
        print(f"File size: {file_size:,} bytes ({file_size/1024/1024:.1f} MB)\n")
        
        # 1. Parse EBML header and find timecode_scale
        timecode_scale = 1_000_000  # default
        first_cluster_pos = None
        
        while True:
            pos = f.tell()
            eid = read_id(f)
            if eid is None:
                break
            
            vint = read_vint(f)
            if vint is None:
                break
            size_len, size, is_unknown = vint
            
            if eid == 0x18538067:  # Segment
                print(f"Segment at pos {pos}")
                continue
            elif eid == 0x1549A966:  # Info
                info_end = f.tell() + size
                while f.tell() < info_end:
                    ie = read_id(f)
                    if ie is None: break
                    iv = read_vint(f)
                    if iv is None: break
                    _, isz, _ = iv
                    if ie == 0x2AD7B1:  # TimecodeScale
                        data = f.read(isz)
                        timecode_scale = read_uint(data)
                        print(f"TimecodeScale: {timecode_scale}")
                    elif ie == 0x4489:  # Duration
                        data = f.read(isz)
                        if isz == 8:
                            dur = struct.unpack('>d', data)[0]
                        elif isz == 4:
                            dur = struct.unpack('>f', data)[0]
                        else:
                            dur = 0
                        print(f"Duration (float): {dur} -> {dur * timecode_scale / 1e9:.3f} seconds")
                    else:
                        f.seek(isz, 1)
            elif eid == 0x1F43B675:  # Cluster
                first_cluster_pos = pos
                print(f"\nFirst Cluster at pos {pos}")
                break
            else:
                if is_unknown:
                    break
                f.seek(size, 1)
        
        if first_cluster_pos is None:
            print("ERROR: No clusters found!")
            return
        
        target_ns = int(target_sec * 1e9)
        target_ticks = target_ns // timecode_scale
        print(f"\nTarget: {target_sec}s = {target_ns} ns = {target_ticks} ticks")
        print(f"TimecodeScale: {timecode_scale} ns")
        
        # 2. Scan all clusters, find the one near the target
        f.seek(first_cluster_pos)
        
        clusters = []
        cluster_count = 0
        problematic_clusters = []
        
        print(f"\n=== Scanning all clusters ===")
        
        while True:
            pos = f.tell()
            if pos >= file_size:
                break
            
            eid = read_id(f)
            if eid is None:
                break
            
            vint = read_vint(f)
            if vint is None:
                break
            size_len, size, is_unknown = vint
            
            if eid == 0x1F43B675:  # Cluster
                data_start = f.tell()
                cluster_count += 1
                
                # Try to read the first 64 bytes (like the Rust code does)
                f.seek(pos)
                raw_buf = f.read(min(64, file_size - pos))
                
                # Simulate the Rust function
                ts_result, debug_info = simulate_rust_read_cluster_timestamp_at(raw_buf, timecode_scale)
                
                # Also try to parse normally
                f.seek(data_start)
                normal_ts = None
                has_timestamp_child = False
                cluster_end = data_start + size if not is_unknown else file_size
                
                # Read first few children
                for _ in range(10):
                    if f.tell() >= cluster_end:
                        break
                    ce = read_id(f)
                    if ce is None:
                        break
                    cv = read_vint(f)
                    if cv is None:
                        break
                    _, csz, _ = cv
                    
                    if ce == 0xE7:  # Timestamp
                        has_timestamp_child = True
                        data = f.read(csz)
                        normal_ts = read_uint(data) * timecode_scale
                        break
                    elif ce == 0xA3 or ce == 0xA0:  # SimpleBlock or BlockGroup
                        # Timestamp should come before blocks
                        break
                    else:
                        f.seek(csz, 1)
                
                ts_sec = normal_ts / 1e9 if normal_ts is not None else None
                
                entry = {
                    'pos': pos,
                    'size': size if not is_unknown else 'unknown',
                    'ts_ns': normal_ts,
                    'ts_sec': ts_sec,
                    'has_timestamp': has_timestamp_child,
                    'rust_result': ts_result,
                    'is_unknown_size': is_unknown,
                }
                clusters.append(entry)
                
                # Check if rust simulation would fail
                if ts_result is None:
                    problematic_clusters.append((entry, debug_info))
                
                # Print progress for clusters near target
                if ts_sec is not None and abs(ts_sec - target_sec) < 30:
                    print(f"\n  ** NEAR TARGET ** Cluster #{cluster_count} at pos {pos}:")
                    print(f"     Normal parse: {ts_sec:.3f}s")
                    print(f"     Rust sim:     {'FAILED' if ts_result is None else f'{ts_result/1e9:.3f}s'}")
                    print(f"     Size: {size}, unknown_size: {is_unknown}")
                    if ts_result is None:
                        for line in debug_info:
                            print(f"       {line}")
                    # Hex dump of first 64 bytes
                    print(f"     First 64 bytes hex:")
                    f.seek(pos)
                    hexdata = f.read(min(64, file_size - pos))
                    for offset in range(0, len(hexdata), 16):
                        hex_part = ' '.join(f'{b:02x}' for b in hexdata[offset:offset+16])
                        ascii_part = ''.join(chr(b) if 32 <= b < 127 else '.' for b in hexdata[offset:offset+16])
                        print(f"       {offset:04x}: {hex_part:<48s}  {ascii_part}")
                
                # Skip to next element
                if is_unknown:
                    # For unknown-size clusters, we need to scan forward
                    # Just scan byte by byte for next cluster header
                    f.seek(data_start)
                    buf = b''
                    CHUNK = 1024*1024
                    found_next = False
                    scan_pos = data_start
                    CLUSTER_ID = bytes([0x1F, 0x43, 0xB6, 0x75])
                    while scan_pos < file_size:
                        f.seek(scan_pos)
                        chunk = f.read(min(CHUNK, file_size - scan_pos))
                        if not chunk:
                            break
                        idx = chunk.find(CLUSTER_ID)
                        if idx >= 0 and scan_pos + idx > pos:
                            f.seek(scan_pos + idx)
                            found_next = True
                            break
                        scan_pos += len(chunk) - 4  # overlap
                    if not found_next:
                        break
                else:
                    f.seek(data_start + size)
            else:
                if is_unknown:
                    break
                f.seek(size, 1) if size > 0 else None
        
        print(f"\n=== Summary ===")
        print(f"Total clusters: {cluster_count}")
        
        if clusters:
            first = clusters[0]
            last = clusters[-1]
            print(f"First cluster: pos={first['pos']}, ts={first['ts_sec']:.3f}s" if first['ts_sec'] is not None else f"First cluster: pos={first['pos']}, ts=NONE")
            print(f"Last cluster:  pos={last['pos']}, ts={last['ts_sec']:.3f}s" if last['ts_sec'] is not None else f"Last cluster:  pos={last['pos']}, ts=NONE")
        
        if problematic_clusters:
            print(f"\n=== PROBLEMATIC CLUSTERS (Rust sim would fail) ===")
            for entry, debug_info in problematic_clusters:
                print(f"\nCluster at pos {entry['pos']}:")
                print(f"  Normal parse timestamp: {entry['ts_sec']}")
                print(f"  Has timestamp child: {entry['has_timestamp']}")
                print(f"  Size: {entry['size']}, unknown: {entry['is_unknown_size']}")
                print(f"  Rust simulation debug:")
                for line in debug_info:
                    print(f"    {line}")
                # Hex dump
                with open(filepath, 'rb') as f2:
                    f2.seek(entry['pos'])
                    hexdata = f2.read(min(128, file_size - entry['pos']))
                    print(f"  First 128 bytes hex:")
                    for offset in range(0, len(hexdata), 16):
                        hex_part = ' '.join(f'{b:02x}' for b in hexdata[offset:offset+16])
                        ascii_part = ''.join(chr(b) if 32 <= b < 127 else '.' for b in hexdata[offset:offset+16])
                        print(f"    {offset:04x}: {hex_part:<48s}  {ascii_part}")
        else:
            print("\nNo problematic clusters found - all clusters have valid timestamps.")
            
        # 3. Show what the binary search would do
        print(f"\n=== Binary search simulation ===")
        # Find clusters that bracket the target
        closest_before = None
        closest_after = None
        for c in clusters:
            if c['ts_sec'] is not None:
                if c['ts_sec'] <= target_sec:
                    if closest_before is None or c['ts_sec'] > closest_before['ts_sec']:
                        closest_before = c
                elif c['ts_sec'] > target_sec:
                    if closest_after is None or c['ts_sec'] < closest_after['ts_sec']:
                        closest_after = c
        
        if closest_before:
            print(f"Closest cluster BEFORE target: pos={closest_before['pos']}, ts={closest_before['ts_sec']:.3f}s")
        if closest_after:
            print(f"Closest cluster AFTER target:  pos={closest_after['pos']}, ts={closest_after['ts_sec']:.3f}s")
        
        # 4. Simulate the binary search midpoint
        lo = first_cluster_pos
        hi = file_size
        mid = lo + (hi - lo) // 2
        print(f"\nBinary search initial: lo={lo}, hi={hi}, mid={mid}")
        
        # Check what's at the midpoint
        with open(filepath, 'rb') as f2:
            # Scan forward from mid for cluster header
            CLUSTER_ID = bytes([0x1F, 0x43, 0xB6, 0x75])
            f2.seek(mid)
            chunk = f2.read(min(819200, file_size - mid))
            idx = chunk.find(CLUSTER_ID)
            if idx >= 0:
                cluster_pos = mid + idx
                print(f"First cluster from mid: pos={cluster_pos}")
                f2.seek(cluster_pos)
                raw = f2.read(64)
                ts, dbg = simulate_rust_read_cluster_timestamp_at(raw, timecode_scale)
                if ts is not None:
                    print(f"  Timestamp: {ts/1e9:.3f}s")
                else:
                    print(f"  FAILED to read timestamp!")
                    for line in dbg:
                        print(f"    {line}")
                    print(f"  Hex dump:")
                    for offset in range(0, len(raw), 16):
                        hex_part = ' '.join(f'{b:02x}' for b in raw[offset:offset+16])
                        print(f"    {offset:04x}: {hex_part}")

if __name__ == "__main__":
    main()
