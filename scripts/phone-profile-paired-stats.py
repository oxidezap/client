#!/usr/bin/env python3
"""Paired median percent differences and percentile bootstrap intervals."""
import csv, statistics, random, sys
rng=random.Random(20261030)
for name in sys.argv[1:]:
 rows=list(csv.DictReader(open(name)))
 for case in ['left','format']:
  rs=[r for r in rows if r['mode']=='timing' and r['case']==case]
  print(name,case)
  for key in ['cold_ns','warm_ns','cpu_us','wall_ns','maxrss_kib']:
   if key not in rs[0]: continue
   vals={rev:{int(r['pair']):int(r[key]) for r in rs if r['rev']==rev} for rev in ['base','head']}
   ratios=[100*(vals['head'][i]/vals['base'][i]-1) for i in vals['base']]
   bs=sorted(statistics.median(rng.choices(ratios,k=len(ratios))) for _ in range(10000))
   print(key,'n',len(ratios),'paired_percent',round(statistics.median(ratios),3),'bootstrap95',round(bs[249],3),round(bs[9749],3))
   for rev in ['base','head']:
    vs=list(vals[rev].values()); q=statistics.quantiles(vs,n=4)
    print(rev,'median',statistics.median(vs),'iqr',q[0],q[2],'range',min(vs),max(vs))
