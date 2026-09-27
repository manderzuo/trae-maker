//! Bounded v2 chat conversion; successful terminal output is published by the
//! caller only after saving the complete result in the execution ledger.

use serde_json::{json,Value};
use std::{collections::BTreeMap,io::{Read,BufRead,BufReader}};

pub(crate) fn aggregate_live<R:Read+Send>(reader:R,id:&str,model:&str,mut emit:impl FnMut(Value))->Result<Value,String> {
    // Take bounds even a single unterminated line. The aggregate and delta
    // copies are also bounded by this entire-wire limit, not only max_tokens.
    const LIMIT:u64=4*1024*1024;
    let mut br=BufReader::new(reader.take(LIMIT+1));
    let mut total=0u64;let mut line=String::new();let mut st=super::SseState::new();
    let mut text=String::new();let mut reasoning=String::new();let mut tools=BTreeMap::<u64,Value>::new();
    let mut usage=None;let mut finish="stop".to_string();let mut done=false;
    loop {
        line.clear();let n=br.read_line(&mut line).map_err(|_|"chat_stream_read_failed")?;
        total+=n as u64;if total>LIMIT {return Err("chat_stream_too_large".into());}if n==0 {break;}
        if line.trim().is_empty() && matches!(st.event.as_str(),"output"|"thought"|"done"|"turn_completion"|"token_usage"|"error")
            && !st.data.is_empty() && !serde_json::from_str::<Value>(&st.data).is_ok_and(|v|v.is_object()) {
            return Err("chat_stream_invalid_event".into());
        }
        let Some(ev)=super::scan_line(&mut st,line.trim_end()) else {continue};
        match ev.event.as_str() {
            "error"=>return Err("chat_upstream_error".into()),
            "done"|"turn_completion"=>{done=true;if !ev.finish_reason.is_empty() {finish=ev.finish_reason;}break;},
            "token_usage"=>usage=ev.usage,
            "output"|"thought"=>{
                if done {return Err("chat_output_after_completion".into());}
                let mut delta=serde_json::Map::new();
                if !ev.response.is_empty() {text.push_str(&ev.response);delta.insert("content".into(),json!(ev.response));}
                if !ev.reasoning.is_empty() {reasoning.push_str(&ev.reasoning);delta.insert("reasoning_content".into(),json!(ev.reasoning));}
                if let Some(calls)=ev.tool_calls {
                    let calls=calls.as_array().ok_or("chat_invalid_tool_calls")?;let mut converted=Vec::new();
                    for (pos,call) in calls.iter().enumerate() {
                        let index=match call.get("index") {Some(v)=>v.as_u64().ok_or("chat_invalid_tool_index")?,None=>pos as u64};
                        if index>=64 {return Err("chat_tool_limit".into());}
                        let function=call.get("function").or_else(||call.get("function_call")).and_then(Value::as_object).ok_or("chat_invalid_tool_function")?;
                        let current=tools.entry(index).or_insert_with(||json!({"id":"","type":"function","function":{"name":"","arguments":""}}));
                        let mut item=json!({"index":index,"type":"function","function":{}});
                        for (key,incoming) in [("id",call.get("id")),("name",function.get("name"))] {
                            if let Some(v)=incoming {
                                let v=v.as_str().ok_or("chat_invalid_tool_identity")?;
                                if !v.is_empty() {
                                    let slot=if key=="id" {&mut current["id"]} else {&mut current["function"]["name"]};
                                    if slot.as_str().is_some_and(|old|!old.is_empty() && old!=v) {return Err("chat_tool_identity_conflict".into());}
                                    *slot=json!(v);
                                    if key=="id" {item["id"]=json!(v);} else {item["function"]["name"]=json!(v);}
                                }
                            }
                        }
                        if let Some(args)=function.get("arguments") {
                            let args=args.as_str().ok_or("chat_invalid_tool_arguments")?;
                            let previous=current["function"]["arguments"].as_str().unwrap();
                            current["function"]["arguments"]=json!(format!("{previous}{args}"));item["function"]["arguments"]=json!(args);
                        }
                        converted.push(item);
                    }
                    if !converted.is_empty() {delta.insert("tool_calls".into(),json!(converted));}
                }
                if !delta.is_empty() {emit(Value::Object(delta));}
            },
            _=>{},
        }
    }
    if !done {return Err("chat_stream_incomplete".into());}
    if tools.values().any(|t|t["id"].as_str()==Some("") || t["function"]["name"].as_str()==Some("")) {return Err("chat_incomplete_tool_identity".into());}
    // SOLO's generic terminal may omit the OpenAI-specific tool stop reason.
    // Preserve length/filter failures, but let agents execute completed calls.
    if !tools.is_empty() && finish=="stop" {finish="tool_calls".into();}
    let mut message=json!({"role":"assistant","content":text});
    if !reasoning.is_empty() {message["reasoning_content"]=json!(reasoning);}
    if !tools.is_empty() {message["tool_calls"]=json!(tools.into_values().collect::<Vec<_>>());}
    let mut result=json!({"id":id,"object":"chat.completion","created":super::now_ts(),"model":model,
        "choices":[{"index":0,"message":message,"finish_reason":finish}]});
    if let Some(usage)=usage {result["usage"]=usage;}Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Cursor,Read};
    struct Gated { first:Cursor<Vec<u8>>, rest:Cursor<Vec<u8>>, seen:std::sync::Arc<std::sync::atomic::AtomicBool> }
    impl Read for Gated {
        fn read(&mut self,b:&mut [u8])->std::io::Result<usize> {
            let n=self.first.read(b)?;if n>0 {return Ok(n);}
            assert!(self.seen.load(std::sync::atomic::Ordering::SeqCst),"first delta must arrive before upstream finishes");self.rest.read(b)
        }
    }
    #[test]
    fn deltas_precede_completion_and_tool_arguments_survive() {
        let seen=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first="event: output\ndata: {\"response\":\"hello\",\"reasoning_content\":\"think\"}\n\n";
        let rest=concat!("event: output\ndata: {\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function_call\":{\"name\":\"download\",\"arguments\":\"{\\\"url\\\":\"}}]}\n\n",
            "event: output\ndata: {\"tool_calls\":[{\"index\":0,\"function_call\":{\"arguments\":\"\\\"https://example.com/v.mp4\\\"}\"}}]}\n\n",
            "event: token_usage\ndata: {\"completion_tokens\":12}\n\nevent: done\ndata: {\"finish_reason\":\"tool_calls\"}\n\n");
        let mut chunks=Vec::new();
        let result=aggregate_live(Gated {first:Cursor::new(first.as_bytes().to_vec()),rest:Cursor::new(rest.as_bytes().to_vec()),seen:seen.clone()},"chat-1","model",|delta| {
            seen.store(true,std::sync::atomic::Ordering::SeqCst);chunks.push(delta);
        }).unwrap();
        assert_eq!(chunks[0],json!({"content":"hello","reasoning_content":"think"}));
        assert_eq!(chunks[1]["tool_calls"][0]["function"]["name"],"download");
        assert_eq!(chunks[2]["tool_calls"][0]["function"]["arguments"],"\"https://example.com/v.mp4\"}");
        let message=&result["choices"][0]["message"];
        assert_eq!(message["tool_calls"],json!([{"id":"call-1","type":"function","function":{"name":"download","arguments":"{\"url\":\"https://example.com/v.mp4\"}"}}]));
        assert_eq!(result["choices"][0]["finish_reason"],"tool_calls");assert_eq!(result["usage"]["completion_tokens"],12);
        assert_eq!(result["model"],"model");
    }
    #[test]
    fn completed_tool_calls_use_tool_finish_but_truncated_calls_stay_truncated() {
        let calls="event: output\ndata: {\"tool_calls\":[{\"id\":\"call-1\",\"function_call\":{\"name\":\"download\",\"arguments\":\"{}\"}}]}\n\n";
        for (terminal,expected) in [("event: done\ndata: {}\n\n","tool_calls"),("event: turn_completion\ndata: {}\n\n","tool_calls"),("event: done\ndata: {\"finish_reason\":\"length\"}\n\n","length")] {
            let result=aggregate_live(Cursor::new(format!("{calls}{terminal}")),"id","model",|_|{}).unwrap();
            assert_eq!(result["choices"][0]["finish_reason"],expected);
        }
    }
    #[test]
    fn explicit_terminal_does_not_wait_for_socket_close_and_malformed_output_is_rejected() {
        struct NeverRead;
        impl Read for NeverRead {fn read(&mut self,_:&mut [u8])->std::io::Result<usize> {panic!("terminal must not wait for socket EOF");}}
        let input=Cursor::new("event: output\ndata: {\"response\":\"done\"}\n\nevent: done\ndata: {}\n\n").chain(NeverRead);
        assert_eq!(aggregate_live(input,"id","model",|_|{}).unwrap()["choices"][0]["message"]["content"],"done");
        assert!(aggregate_live(Cursor::new("event: output\ndata: malformed\n\nevent: done\ndata: {}\n\n"),"id","model",|_|{}).is_err());
    }
    #[test]
    fn partial_error_and_conflicting_tool_identity_never_report_success() {
        for raw in ["event: output\ndata: {\"response\":\"partial\"}\n\n",
            "event: error\ndata: {\"code\":42,\"message\":\"failure\"}\n\nevent: done\ndata: {}\n\n",
            "event: output\ndata: {\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function_call\":{\"name\":\"one\"}},{\"index\":0,\"id\":\"b\",\"function_call\":{\"name\":\"two\"}}]}\n\nevent: done\ndata: {}\n\n"] {
            assert!(aggregate_live(Cursor::new(raw),"id","model",|_|{}).is_err());
        }
        let raw=format!("event: output\ndata: {}\n\nevent: done\ndata: {{}}\n\n",json!({"response":"x".repeat(4*1024*1024)}));
        assert!(aggregate_live(Cursor::new(raw),"id","model",|_|{}).is_err());
    }
}
