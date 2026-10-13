#!/usr/bin/env python3
"""Independent import-resolution ground truth for lang8 corpora.

Imports are found by regex over comment-stripped source (NOT via astrolabe),
and the expected in-repo target set is computed from the file tree and build
files. Compares against `import_dump` TSV (from, lang, spec, resolved).
Pair with `cargo run --release --example import_dump -- <repo> > dump.tsv`.
Usage: scripts/import_recall_gt.py <lang> <corpus_root> <dump.tsv> [--misses N]
"""
import os, re, sys, subprocess, collections

lang, root, dump = sys.argv[1], sys.argv[2], sys.argv[3]
show = int(sys.argv[sys.argv.index('--misses')+1]) if '--misses' in sys.argv else 0

def files_list():
    out = subprocess.run(['git','-C',root,'ls-files'],capture_output=True,text=True).stdout.split('\n')
    if not out or out==['']:
        out=[]
        for d,_,fs in os.walk(root):
            for f in fs: out.append(os.path.relpath(os.path.join(d,f),root))
    skip = re.compile(r'(^|/)(node_modules|\.git|build|dist|target|vendor|Pods|third_party|3rd-party|out)/')
    return [f for f in out if f and not skip.search(f) and os.path.isfile(os.path.join(root,f))]
FILES = files_list(); FSET=set(FILES)
def read(f):
    try: return open(os.path.join(root,f),encoding='utf-8',errors='replace').read()
    except Exception: return ''
def norm(p):
    parts=[]
    for s in p.split('/'):
        if s in ('','.'): continue
        if s=='..':
            if not parts: return None
            parts.pop()
        else: parts.append(s)
    return '/'.join(parts)
def jn(d,r): return norm(d+'/'+r if d else r)
def dirn(f): return f.rsplit('/',1)[0] if '/' in f else ''

def strip_c(s):  # remove // and /* */ comments (keeps strings roughly)
    s=re.sub(r'/\*.*?\*/',lambda m:'\n'*m.group().count('\n'),s,flags=re.S)
    return re.sub(r'//[^\n]*','',s)

gt=[]  # (from, spec, expected_set or None=external)

if lang=='kotlin':
    decl=collections.defaultdict(set)  # (pkg,name)->files
    for f in FILES:
        if f.endswith(('.kt','.kts')):
            s=strip_c(read(f)); m=re.search(r'^\s*package\s+([\w.`]+)',s,re.M); pkg=m.group(1).replace('`','') if m else ''
            for m in re.finditer(r'^(?:@\S+\s+|(?:public|internal|private|expect|actual|data|sealed|abstract|open|enum|annotation|inline|value|fun|suspend|operator|infix|tailrec|external|const|lateinit|inner)\s+)*(?:class|interface|object|fun|val|var|typealias)\s+(?:<[^>]*>\s*)?(?:[\w.<>?, ]+\.)?`?(\w+)`?',s,re.M):
                decl[(pkg,m.group(1))].add(f)
        elif f.endswith('.java'):
            s=strip_c(read(f)); m=re.search(r'^\s*package\s+([\w.]+)',s,re.M)
            if m: decl[(m.group(1),os.path.basename(f)[:-5])].add(f)
    for f in FILES:
        if not f.endswith(('.kt','.kts')): continue
        for m in re.finditer(r'^\s*import\s+([\w`]+(?:\.[\w`]+)*)(\.\*)?(?:\s+as\s+\w+)?',strip_c(read(f)),re.M):
            spec=m.group(1).replace('`',''); segs=spec.split('.'); exp=None
            rng = range(len(segs)-1,0,-1) if not m.group(2) else range(len(segs)-1,0,-1)
            if m.group(2):
                # star import: in-repo only if it names a type
                for i in range(len(segs)-1,0,-1):
                    k=('.'.join(segs[:i]),segs[i])
                    if k in decl: exp=decl[k]; break
            else:
                for i in range(len(segs)-1,0,-1):
                    k=('.'.join(segs[:i]),segs[i])
                    if k in decl: exp=decl[k]; break
            gt.append((f,spec,exp))

elif lang=='ruby':
    libroots=sorted({f[:f.index('/lib/')+4] if '/lib/' in f else 'lib' for f in FILES if f.endswith('.rb') and (f.startswith('lib/') or '/lib/' in f)})
    for f in FILES:
        if not (f.endswith(('.rb','.gemspec','.rake')) or os.path.basename(f) in ('Rakefile','Gemfile')): continue
        s=re.sub(r'^=begin.*?^=end','',read(f),flags=re.S|re.M)
        s=re.sub(r'<<[~-]?([\'"]?)([A-Z_]+)\1.*?^\s*\2$','',s,flags=re.S|re.M)
        for line in s.split('\n'):
            code=re.sub(r'#(?!\{).*','',line)
            for m in re.finditer(r'(?<![\w.])(require_relative|require|load|autoload\s+:\w+\s*,)\s*\(?\s*([\'"])([^\'"#]+)\2',code):
                kind,spec=m.group(1),m.group(3)
                sp=spec[:-3] if spec.endswith('.rb') else spec
                exp=None
                if kind=='require_relative' or sp.startswith(('./','../')):
                    p=jn(dirn(f),sp+'.rb'); exp={p} if p in FSET else None
                else:
                    c=[jn(r,sp+'.rb') for r in libroots]+[jn(r,sp+'.rb') for r in ('test','spec','')]
                    c=[x for x in c if x in FSET]
                    if c:
                        # prefer same gem (longest common lib root prefix)
                        own=[x for x in c if any(f.startswith(r[:-3]) and x.startswith(r+'/') for r in libroots if r!='lib')]
                        exp=set(own or c[:1])
                key=('relative:'+spec) if kind=='require_relative' else spec
                gt.append((f,spec,exp))

elif lang=='shell':
    shs=[]
    for f in FILES:
        b=os.path.basename(f); src=read(f)
        if f.endswith(('.sh','.bash')) or ('.' not in b and re.match(r'#!.*\b(ba)?sh\b',src)):
            shs.append((f,src))
    for f,src in shs:
        for line in src.split('\n'):
            code=line.strip()
            if code.startswith('#'): continue
            for m in re.finditer(r'(?:^|[;&|]\s*|\bthen\s+|\bdo\s+|&&\s*|\|\|\s*)(?:\\?\.|source)\s+("[^"]*"|\'[^\']*\'|[^\s;&|)]+)',code):
                raw=m.group(1).strip('"\''); spec=raw
                exp=None
                tail=None
                d=dirn(f)
                mm=re.match(r'^(?:\$\{?BASH_SOURCE(?:\[0\])?%/\*\}?|\$\(dirname\s+"?\$\{?(?:0|BASH_SOURCE(?:\[0\])?)\}?"?\)|\$\{?(?:DIR|SCRIPT_DIR|HERE|THIS_DIR|SCRIPTDIR)\}?)/(.+)$',raw)
                if mm and '$' not in mm.group(1):
                    p=jn(d,mm.group(1)); exp={p} if p in FSET else None
                elif '$' not in raw and '`' not in raw:
                    if not raw.startswith(('/','~')):
                        for base in (d,''):
                            p=jn(base,raw)
                            if p in FSET: exp={p}; break
                else:
                    t=re.sub(r'^.*(\$\{[^}]*\}|\$\([^)]*\)|\$\w+)"?','',raw)
                    if t.startswith('/') and '$' not in t and '*' not in t:
                        t=t[1:]; base=d
                        while True:
                            p=jn(base,t)
                            if p and p in FSET: exp={p}; break
                            if not base: break
                            base=dirn(base)
                        if exp is None:
                            c=[x for x in FILES if x==t or x.endswith('/'+t)]
                            if len(c)==1: exp=set(c)
                gt.append((f,spec,exp))

elif lang=='lua':
    roots=['']+[r for r in ('lua','src','lib','script') if any(x.startswith(r+'/') for x in FILES)]
    for f in FILES:
        if not f.endswith('.lua'): continue
        s=re.sub(r'--\[(=*)\[.*?\]\1\]','',read(f),flags=re.S); s=re.sub(r'--[^\n]*','',s)
        s=re.sub(r'(?<!require)(?<!require )(?<!require\()\[(=*)\[.*?\]\1\]','',s,flags=re.S)
        for m in re.finditer(r'(?<![\w.:])(require|dofile|loadfile)\s*\(?\s*(?:([\'"])([^\'"\n]+)\2(?!\s*\.\.)|\[\[([^\]]+)\]\])',s):
            spec=m.group(3) or m.group(4); exp=None
            if m.group(1)!='require' or spec.endswith('.lua'):
                for base in ('',dirn(f)):
                    p=jn(base,spec.lstrip('./') if spec.startswith('./') else spec)
                    if p in FSET: exp={p}; break
            elif '/' not in spec:
                rel=spec.replace('.','/')
                c=[]
                for r in roots:
                    for t in (rel+'.lua',rel+'/init.lua'):
                        p=jn(r,t)
                        if p in FSET: c.append(p)
                if not c:  # package.path set to own/ancestor dir
                    d=dirn(f)
                    while True:
                        for t in (rel+'.lua',rel+'/init.lua'):
                            p=jn(d,t)
                            if p in FSET: c.append(p)
                        if c or not d: break
                        d=dirn(d)
                if c: exp={c[0]}
            gt.append((f,spec,exp))

elif lang=='objc':
    hdrs=collections.defaultdict(list)
    for f in FILES:
        if f.endswith(('.h','.hh','.hpp')): hdrs[os.path.basename(f)].append(f)
    for f in FILES:
        if not f.endswith(('.m','.mm','.h')): continue
        src=read(f)
        if f.endswith('.h') and not re.search(r'@interface|@protocol|#import|NS_ASSUME|@class',src): continue
        for m in re.finditer(r'^\s*#\s*(?:import|include)\s*([<"])([^>"]+)[>"]',strip_c(src),re.M):
            q,spec=m.group(1),m.group(2); exp=None
            p=jn(dirn(f),spec)
            if q=='"' and p in FSET: exp={p}
            elif spec in FSET: exp={spec}
            else:
                c=hdrs.get(os.path.basename(spec),[])
                suf=[x for x in c if x.endswith('/'+spec)]
                c=suf or c
                if c:
                    # nearest by shared directory prefix
                    def sh(x):
                        a=dirn(f).split('/'); b=x.split('/'); n=0
                        while n<min(len(a),len(b)) and a[n]==b[n]: n+=1
                        return n
                    best=max(sh(x) for x in c); exp={x for x in c if sh(x)==best}
            gt.append((f,spec,exp))

elif lang=='swift':
    mods={}
    for f in FILES:
        if os.path.basename(f)=='Package.swift':
            s=strip_c(read(f)); base=dirn(f)
            for m in re.finditer(r'\.(target|executableTarget|testTarget|macro|plugin)\s*\(\s*name:\s*"([^"]+)"(.*?)(?=\n\s*\.\w+\(\s*name:|\Z)',s,re.S):
                pm=re.search(r'\bpath:\s*"([^"]+)"',m.group(3).split('dependencies')[0] if False else m.group(3))
                d=pm.group(1) if pm else (('Tests/' if m.group(1)=='testTarget' else 'Sources/')+m.group(2))
                mods.setdefault(m.group(2),jn(base,d))
    for f in FILES:
        if not f.endswith('.swift') or os.path.basename(f).startswith('Package'): continue
        for m in re.finditer(r'^\s*(?:@\w+(?:\([^)]*\))?\s+)*import\s+(?:(?:struct|class|enum|protocol|func|var|let|typealias)\s+)?(\w+)',strip_c(read(f)),re.M):
            spec=m.group(1); d=mods.get(spec); exp=None
            if d is None:
                for c in ('Sources/'+spec, spec):
                    if any(x.startswith(c+'/') and x.endswith('.swift') for x in FILES): d=c; break
            if d is not None:
                exp={x for x in FILES if x.startswith(d+'/') and x.endswith('.swift')} or None
            gt.append((f,spec,exp))

# ---- compare
res=collections.defaultdict(list)
for line in open(dump):
    p=line.rstrip('\n').split('\t')
    if len(p)<4: continue
    sp=p[2]
    for pre in ('relative:',):
        if sp.startswith(pre): sp=sp[len(pre):]
    if lang=='swift': sp=sp.split('.')[0]
    if lang=='kotlin': sp=sp.replace('`','')
    res[(p[0],sp.strip('"\'<>'))].append(p[3])
inrepo=[g for g in gt if g[2]]
ok=0; miss=collections.Counter(); ex=[]
for f,spec,exp in inrepo:
    cand=res.get((f,spec.strip('"\'<>')))
    if cand is None:
        cand=res.get((f,spec.strip('"\'').replace('\\.','.')))
    if cand is None: miss['not_extracted']+=1; ex.append(('not_extracted',f,spec,'',sorted(exp)[:2])); continue
    r=cand.pop(0) if len(cand)>1 else cand[0]
    if r in exp: ok+=1
    elif r=='': miss['unresolved']+=1; ex.append(('unresolved',f,spec,r,sorted(exp)[:2]))
    else: miss['wrong_file']+=1; ex.append(('wrong_file',f,spec,r,sorted(exp)[:2]))
n=len(inrepo)
print(f"{lang}\t{os.path.basename(root.rstrip('/'))}\tin-repo={n}\tcorrect={ok}\trate={100*ok/n if n else 100:.2f}%\t{dict(miss)}")
for e in ex[:show]: print('   ',*e,sep=' | ')
