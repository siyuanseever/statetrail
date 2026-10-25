#!/usr/bin/env python3
"""给 Rust 版搭隔离的演示 herdr：借用 Python 版 herdr/demo/gif.py 的 --stage（假 $HOME、私有 tmux、假 Claude），
只把 link 进去的插件目录换成 herdr-rs/。不改冻结的 Python 版。

    cargo build --release && mkdir -p bin && cp target/release/statetrail bin/.new && mv bin/.new bin/statetrail && python3 demo/stage.py
    tmux -L herdrdemo attach -t demo        # 自己看；收掉：tmux -L herdrdemo kill-server 再 herdr server stop
"""
import sys
from pathlib import Path

RS = Path(__file__).resolve().parent.parent
DEMO = RS.parent / "herdr" / "demo"
sys.path.insert(0, str(DEMO))
sys.argv = [str(DEMO / "gif.py"), "--stage"]
# gif.py 最后一行无条件调用 main()：import 它就会先用 Python 版布置一遍。读源码、去掉那一行再执行
src = (DEMO / "gif.py").read_text()
assert src.rstrip().endswith("\nmain()")
gif = {"__file__": str(DEMO / "gif.py"), "__name__": "gif"}
exec(compile(src.rstrip()[: -len("main()")], str(DEMO / "gif.py"), "exec"), gif)
gif["PLUGIN"] = RS
gif["STAGE"] = True
gif["main"]()
