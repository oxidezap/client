#!/usr/bin/env python3
"""Compare extracted production fill helpers plus selected local file reads.

Uses the #280 allocator/C launcher infrastructure, not the connected application.
Usage: python3 scripts/measure-demand-media.py BASE HEAD
"""
import csv
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile

helpers = runpy.run_path(str(Path(__file__).with_name('measure-frame-media.py')))


def tree(ref, path):
    return Path(path).read_text() if ref == 'WORKTREE' else subprocess.check_output(['git', 'show', f'{ref}:{path}'], text=True)


def source(ref, bounded):
    frames = tree(ref, 'crates/gui/src/session/frames.rs')
    fill = frames[frames.index('fn fill(media:'):frames.index('\n#[cfg(test)]', frames.index('fn fill(media:'))]
    shim = helpers['SHIM'].replace(
        'trait MediaCache {fn read(&self,key:&str)->Result<Arc<Vec<u8>>,String>;}',
        'trait MediaCache {fn defers_message_media(&self)->bool {true} fn read(&self,key:&str)->Result<Arc<Vec<u8>>,String>;}')
    main = helpers['MAIN'].replace('let heap=args[4]=="heap";', 'let heap=args[4]=="heap";let visible:usize=args[5].parse().unwrap();')
    main = main.replace(' FILL_ROWS', '''
 fill_many(rows.iter_mut(), &cache);
 // The cache-only read/adoption primitive for the requested visible subset.
 // This deliberately excludes the app controller, tasks and view rendering.
 for row in rows.iter_mut().take(visible).flatten() {
  if row.data.is_empty() {
   let key=row.cache_key.take().unwrap();
   let bytes=read_bounded(&cache.path.join(&key), 48*1024*1024).unwrap();
   cache.reads.fetch_add(1,Relaxed);cache.bytes.fetch_add(bytes.len(),Relaxed);
   row.adopt_full_bytes(Arc::new(bytes));
  }
 }
''')
    main = main.replace('assert!(rows.iter().all(|m|m.as_ref().unwrap().data.len()==size));', 'assert!(rows.iter().take(visible).all(|m|m.as_ref().unwrap().data.len()==size));')
    return shim + fill + bounded + main


def main():
    base, head = sys.argv[1:]
    media = tree(head, 'crates/gui/src/session/media.rs')
    start = media.index('fn read_bounded(')
    bounded = media[start:media.index('\n/// Somewhere blocking', start)]
    with tempfile.TemporaryDirectory(prefix='demand-media-') as tmp:
        path = Path(tmp)
        runner = path / 'runner'
        runner_c = path / 'runner.c'
        runner_c.write_text(helpers['RUNNER'])
        subprocess.run(['cc', '-O2', str(runner_c), '-o', str(runner)], check=True)
        bins = {}
        for variant, ref in [('base', base), ('head', head)]:
            src = path / f'{variant}.rs'
            src.write_text(source(ref, bounded))
            binary = path / variant
            subprocess.run(['rustc', '--edition=2024', '-O', str(src), '-o', str(binary)], check=True)
            bins[variant] = binary
        w = csv.writer(sys.stdout)
        w.writerow(['sample','mode','visible','size','variant','elapsed_ns','reads','read_bytes','retained_payload_bytes','additional_peak_requested_heap','allocation_calls','process_user_s','process_system_s','peak_rss_kib'])
        for size in [1024 * 1024, 128]:
            for i in range(12):
                (path / str(i)).write_bytes(bytes([i]) * size)
            for visible in [0, 2, 12]:
                for mode in ['time', 'heap']:
                    for sample in range(-1, 11):
                        for variant in (['base','head'] if sample % 2 == 0 else ['head','base']):
                            result = subprocess.run([str(runner), str(bins[variant]), tmp, '12', str(size), mode, str(visible)], capture_output=True, text=True, check=True)
                            if sample >= 0:
                                w.writerow([sample, mode, visible, size, variant, *result.stdout.strip().split(','), *result.stderr.split(',')])


if __name__ == '__main__':
    main()
