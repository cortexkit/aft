#!/usr/bin/env python3
"""Loopback-only OpenAI-compatible server for checked-in benchmark vectors."""
from __future__ import annotations
import argparse, hashlib, json, math, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

from search_quality_lib import EVIDENCE_SHA, InputFault, ensure_loopback


def text_hash(text: str) -> str: return hashlib.sha256(text.encode()).hexdigest()
def query_key(text: str, template: str) -> str: return f"query:{text_hash(text)}:{template}"
def corpus_key(text: str, template: str) -> str: return f"corpus:{EVIDENCE_SHA}:{text_hash(text)}:{template}"

def fixture_vector(text: str) -> list[float]:
    raw = hashlib.sha256(text.encode()).digest()
    values = [(raw[index] - 127.5) / 127.5 for index in range(8)]
    norm = math.sqrt(sum(value * value for value in values))
    return [round(value / norm, 8) for value in values]


class Server(ThreadingHTTPServer):
    def __init__(self, address: tuple[str, int], vectors: dict[str, list[float]], template: str, log: Path, *, record_missing: bool = False, query_texts: set[str] | None = None):
        super().__init__(address, Handler); self.vectors=vectors; self.template=template; self.log=log; self.record_missing=record_missing; self.query_texts=query_texts or set()

class Handler(BaseHTTPRequestHandler):
    def log_message(self, format: str, *args: Any) -> None: pass
    def do_POST(self) -> None:
        if self.path != "/v1/embeddings": self.send_error(404); return
        server = self.server
        if not isinstance(server, Server):
            self._json(500, {"error": "invalid_server"}); return
        try:
            length=int(self.headers.get("content-length","0")); payload=json.loads(self.rfile.read(length)); values=payload.get("input")
            texts=[values] if isinstance(values,str) else values
            if not isinstance(texts,list) or any(not isinstance(item,str) for item in texts): raise ValueError("input")
            output=[]; requests=[]
            for index,text in enumerate(texts):
                qkey=query_key(text,server.template); ckey=corpus_key(text,server.template)
                key=qkey if qkey in server.vectors else ckey
                if key not in server.vectors:
                    if not server.record_missing:
                        self._json(422,{"error":f"vector_missing:{qkey}|{ckey}"}); return
                    key = qkey if text in server.query_texts else ckey
                    server.vectors[key] = fixture_vector(text)
                vector=server.vectors[key]
                if not vector or any(not isinstance(item,(int,float)) for item in vector): raise ValueError("vector")
                output.append({"object":"embedding","index":index,"embedding":vector}); requests.append(key)
            with server.log.open("a",encoding="utf-8") as handle:
                for key in requests: handle.write(key+"\n")
            self._json(200,{"object":"list","model":payload.get("model","aft-search-fixture-v1"),"data":output})
        except (ValueError,json.JSONDecodeError) as error: self._json(400,{"error":f"invalid_request:{error}"})
    def _json(self,status:int,payload:dict[str,Any])->None:
        data=json.dumps(payload,separators=(",",":")).encode(); self.send_response(status); self.send_header("content-type","application/json"); self.send_header("content-length",str(len(data))); self.end_headers(); self.wfile.write(data)

def main()->int:
    parser=argparse.ArgumentParser(); parser.add_argument("--vectors",required=True); parser.add_argument("--host",default="127.0.0.1"); parser.add_argument("--port",type=int,default=0); parser.add_argument("--log",required=True); parser.add_argument("--check-key")
    args=parser.parse_args()
    try:
        ensure_loopback(args.host); pack=json.loads(Path(args.vectors).read_text()); template=str(pack["embed_template_version"]); vectors=pack["vectors"]
        if args.check_key:
            if args.check_key not in vectors: raise InputFault(f"vector_missing:{args.check_key}")
            return 0
        server=Server((args.host,args.port),vectors,template,Path(args.log)); print(json.dumps({"host":args.host,"port":server.server_port}),flush=True); server.serve_forever()
    except (InputFault,OSError,KeyError,json.JSONDecodeError) as error: print(str(error),file=sys.stderr); return 2
    return 0
if __name__=="__main__": raise SystemExit(main())
