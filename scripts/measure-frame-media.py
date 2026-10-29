#!/usr/bin/env python3
"""Isolate production fill loops with a filesystem cache and synthetic descriptors.

No GUI/IPC/decoder cost is included. Rust shims implement only the domain members
used by fill; the fill bodies themselves are extracted verbatim from the ref.
Builds with rustc -O. Writes CSV to stdout; build products and media live in /tmp.
Usage: python3 scripts/measure-frame-media.py BASE HEAD
"""
import csv
from pathlib import Path
import subprocess
import sys
import tempfile

SHIM = r'''
#![allow(dead_code)]
use std::{sync::Arc, alloc::{GlobalAlloc, Layout, System}, sync::atomic::{AtomicBool,AtomicUsize,Ordering::Relaxed}};
macro_rules! debug { ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }}; }
static ENABLED:AtomicBool=AtomicBool::new(false);
static LIVE:AtomicUsize=AtomicUsize::new(0);
static PEAK:AtomicUsize=AtomicUsize::new(0);
static CALLS:AtomicUsize=AtomicUsize::new(0);
struct Alloc;
fn add(n:usize) { if ENABLED.load(Relaxed) { let v=LIVE.fetch_add(n,Relaxed)+n; PEAK.fetch_max(v,Relaxed); CALLS.fetch_add(1,Relaxed); } }
unsafe impl GlobalAlloc for Alloc {
 unsafe fn alloc(&self,l:Layout)->*mut u8 {let p=unsafe{System.alloc(l)};if !p.is_null(){add(l.size())}p}
 unsafe fn dealloc(&self,p:*mut u8,l:Layout){if ENABLED.load(Relaxed){LIVE.fetch_sub(l.size(),Relaxed);}unsafe{System.dealloc(p,l)}}
 unsafe fn realloc(&self,p:*mut u8,l:Layout,n:usize)->*mut u8 {let q=unsafe{System.realloc(p,l,n)};if !q.is_null()&&ENABLED.load(Relaxed){LIVE.fetch_sub(l.size(),Relaxed);add(n)}q}
}
#[global_allocator] static ALLOC:Alloc=Alloc;
struct MediaContent{data:Arc<Vec<u8>>,cache_key:Option<String>,preview_cache_key:Option<String>}
impl MediaContent {fn adopt_full_bytes(&mut self,b:Arc<Vec<u8>>){self.data=b;}}
trait MediaCache {fn read(&self,key:&str)->Result<Arc<Vec<u8>>,String>;}
struct Directory {path:std::path::PathBuf, reads:AtomicUsize, bytes:AtomicUsize}
impl MediaCache for Directory {fn read(&self,key:&str)->Result<Arc<Vec<u8>>,String>{let b=std::fs::read(self.path.join(key)).map_err(|e|e.to_string())?;self.reads.fetch_add(1,Relaxed);self.bytes.fetch_add(b.len(),Relaxed);Ok(Arc::new(b))}}
'''
MAIN = r'''
fn main(){
 let args:Vec<_>=std::env::args().collect();let unique:usize=args[2].parse().unwrap();let size:usize=args[3].parse().unwrap();let heap=args[4]=="heap";
 let cache=Directory{path:args[1].clone().into(),reads:AtomicUsize::new(0),bytes:AtomicUsize::new(0)};
 // Each operation retains 12 rows, as a history list would. Exclude setup.
 let mut rows:Vec<_>=(0..12).map(|i|Some(MediaContent{data:Arc::new(Vec::new()),cache_key:Some((i%unique).to_string()),preview_cache_key:None})).collect();
 // Existing descriptors may deallocate during fill, so use a live-heap offset
 // to account for their setup allocations without counting them as new work.
 LIVE.store(1<<30,Relaxed);PEAK.store(1<<30,Relaxed);CALLS.store(0,Relaxed);ENABLED.store(heap,Relaxed);
 let start=std::time::Instant::now();
 FILL_ROWS
 let elapsed=start.elapsed().as_nanos();ENABLED.store(false,Relaxed);
 assert!(rows.iter().all(|m|m.as_ref().unwrap().data.len()==size));
 let retained:usize=rows.iter().enumerate().filter(|(i,m)|!rows[..*i].iter().any(|p|Arc::ptr_eq(&p.as_ref().unwrap().data,&m.as_ref().unwrap().data))).map(|(_,m)|m.as_ref().unwrap().data.len()).sum();
 println!("{},{},{},{},{},{}",elapsed,cache.reads.load(Relaxed),cache.bytes.load(Relaxed),retained,PEAK.load(Relaxed)-(1<<30),CALLS.load(Relaxed));
 std::hint::black_box(rows);
}
'''
RUNNER = r'''
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc,char**argv){
 if(argc<2)return 2;
 pid_t p=fork();if(p<0)return 2;
 if(!p){execv(argv[1],argv+1);_exit(127);}
 int status;struct rusage u;
 if(wait4(p,&status,0,&u)<0)return 2;
 fprintf(stderr,"%f,%f,%ld",u.ru_utime.tv_sec+u.ru_utime.tv_usec/1e6,u.ru_stime.tv_sec+u.ru_stime.tv_usec/1e6,u.ru_maxrss);
 return WIFEXITED(status)?WEXITSTATUS(status):2;
}
'''

def source(ref):
    s = subprocess.check_output(['git','show',ref+':crates/gui/src/session/frames.rs'],text=True) if ref != 'WORKTREE' else Path('crates/gui/src/session/frames.rs').read_text()
    start = s.index('fn fill(media:')
    end = s.index('\n#[cfg(test)]',start)
    body=s[start:end]
    call='fill_many(rows.iter_mut(), &cache);' if 'fn fill_many' in body else 'for row in &mut rows { fill(row, &cache); }'
    return SHIM+body+MAIN.replace('FILL_ROWS',call)

def main():
    base, head=sys.argv[1:]
    with tempfile.TemporaryDirectory(prefix='frame-media-') as tmp:
        path=Path(tmp)
        runner_source=path/'runner.c';runner_source.write_text(RUNNER)
        runner=path/'runner'
        subprocess.run(['cc','-O2',str(runner_source),'-o',str(runner)],check=True)
        bins={}
        for name, ref in [('base',base),('head',head)]:
            src=path/(name+'.rs');src.write_text(source(ref));binary=path/name
            subprocess.run(['rustc','--edition=2024','-O',str(src),'-o',str(binary)],check=True)
            bins[name]=binary
        w=csv.writer(sys.stdout, lineterminator='\n');w.writerow(['sample','mode','unique','size','variant','elapsed_ns','reads','read_bytes','retained_payload_bytes','additional_peak_requested_heap','allocation_calls','process_user_s','process_system_s','peak_rss_kib'])
        for unique,size in [(12,1024*1024),(1,1024*1024),(3,1024*1024),(12,128)]:
            for i in range(12): (path/str(i)).write_bytes(bytes([i])*size)
            for mode in ['time','heap']:
                for sample in range(-1,9):
                    order=['base','head'] if sample%2==0 else ['head','base']
                    for variant in order:
                        # Fork from a tiny C parent after exec has reset the
                        # inherited Python RSS floor. rusage belongs to Rust.
                        result=subprocess.run([str(runner),str(bins[variant]),tmp,str(unique),str(size),mode],capture_output=True,text=True,check=True)
                        if sample>=0: w.writerow([sample,mode,unique,size,variant,*result.stdout.strip().split(','),*result.stderr.split(',')])
if __name__=='__main__': main()
