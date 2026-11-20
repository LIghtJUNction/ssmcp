//! Interactive chat attachment. The remote shell is only selected explicitly.
use super::*;
use directory::Selected;

#[derive(Default)]
struct Draft {
    text: String,
    cursor: usize,
    attempt: Option<(String, String)>,
}
impl Draft {
    fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }
    fn left(&mut self) {
        if let Some((at, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = at;
        }
    }
    fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }
    fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.text.drain(self.cursor..end);
    }
    fn display(text: &str) -> String {
        text.chars()
            .map(|c| match c {
                '\n' => '↵',
                '\t' => '⇥',
                c => c,
            })
            .collect()
    }
    fn draw(&self) {
        let tail = Self::display(&self.text[self.cursor..]);
        eprint!("\r\x1b[2Kchat> {}", Self::display(&self.text));
        let width = unicode_width::UnicodeWidthStr::width(tail.as_str());
        if width > 0 {
            eprint!("\x1b[{width}D");
        }
        let _ = io::stderr().flush();
    }
}
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
fn output(text: &str, draft: &Draft, read_only: bool) {
    eprint!("\r\x1b[2K");
    println!("{}", clean(text));
    if !read_only {
        draft.draw();
    }
}

enum Update {
    History(Value),
    Outputs(Value),
    Error(String),
}
async fn observe(
    api: Api,
    credential: Credential,
    target: directory::Target,
    tx: tokio::sync::mpsc::Sender<Update>,
) {
    let mut cursor = None;
    let mut generation = None;
    let mut output_cursor = 0u64;
    let mut output_generation = None;
    loop {
        let result:Result<()>=async {
            let mut args=json!({"action":"thread","client_id":target.client_id,"limit":32});
            if let Some(thread)=&target.thread_id{args["agent_id"]=json!(thread);}
            else if target.kind==directory::Kind::Thread{args["main"]=json!(true);}
            if let Some(cursor)=&cursor{args["cursor"]=cursor.clone();}
            if let Some(generation)=&generation{args["generation"]=generation.clone();}
            let page=api.call(&credential,&target.client_id,"ssmcp.ui.history",args).await?;
            cursor=Some(page.get("next_cursor").filter(|v|v.is_u64()).ok_or("chat history omitted its cursor")?.clone());
            generation=Some(page.get("generation").filter(|v|v.is_string()).ok_or("chat history omitted its generation")?.clone());
            let history_more=page["has_more"]==true;
            tx.send(Update::History(page)).await.map_err(|_|"chat attachment closed")?;
            let mut args=json!({"action":"outputs","client_id":target.client_id,"cursor":output_cursor,"limit":64});
            if let Some(thread)=&target.thread_id{args["thread_id"]=json!(thread);}
            else if target.kind==directory::Kind::Thread{args["main"]=json!(true);}
            if let Some(generation)=&output_generation{args["generation"]=generation.clone();}
            let page=api.post("/api/client/io",Some(&credential.token),args).await?;
            let next=page["next_cursor"].as_u64().ok_or("chat output omitted its cursor")?;
            if next<output_cursor{return Err("chat output cursor moved backwards".into());}
            let mut previous=output_cursor;
            for output in page["outputs"].as_array().ok_or("chat output page has no outputs")? {
                let seq=output["global_sequence"].as_u64().ok_or("chat output omitted its sequence")?;
                if seq<=previous || seq>next{return Err("chat output sequence is repeated or outside its cursor".into());}
                previous=seq;
                if output["client_id"].as_str()!=Some(&target.client_id) || (target.kind==directory::Kind::Thread && output["thread_id"].as_str()!=target.thread_id.as_deref()) {return Err("chat output belongs to a different target".into());}
            }
            output_cursor=next;
            output_generation=Some(page.get("generation").filter(|v|v.is_string()).ok_or("chat output omitted its generation")?.clone());
            let output_more=page["has_more"]==true;
            tx.send(Update::Outputs(page)).await.map_err(|_|"chat attachment closed")?;
            if !history_more && !output_more{tokio::time::sleep(Duration::from_millis(500)).await;}
            Ok(())
        }.await;
        if let Err(error) = result {
            let _ = tx.send(Update::Error(error.to_string())).await;
            break;
        }
    }
}

pub(super) async fn run(
    api: &Api,
    credential: &Credential,
    selection: Selected,
    read_only: bool,
) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("chat attach requires a terminal; use send and wait for pipelines".into());
    }
    if selection.recipients.is_empty() {
        return Err(
            "this server has no chat I/O directory; update it or select an explicit --terminal PTY"
                .into(),
        );
    }
    let target = selection.entry.target.clone();
    let mut keyboard = input::Keyboard::open(read_only)?;
    let mut decoder = input::Utf8Input::default();
    let mut draft = Draft::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let _reader = Reader(tokio::spawn(observe(
        api.clone(),
        credential.clone(),
        target.clone(),
        tx,
    )));
    let mut terminate = platform::terminate_signal()?;
    eprintln!(
        "Attached to chat {} · {} · Ctrl+] / Ctrl+C detaches; requests keep running",
        safe(&target.id()),
        if read_only {
            "read-only"
        } else {
            "Enter sends input"
        }
    );
    if !read_only {
        draft.draw();
    }
    loop {
        tokio::select! {
            _=terminate.recv()=>break,
            update=rx.recv()=>match update.ok_or("chat observer ended")? {
                Update::Error(message)=>return Err(message.into()),
                Update::History(page)=>{
                    if page["retention_gap_possible"]==true || page["truncated_since_cursor"]==true{eprintln!("\r\x1b[2KSome earlier history may be outside the retained process window.");}
                    for event in page["events"].as_array().ok_or("chat history page has no events")? {
                        if let Some(text)=event["text"].as_str().filter(|text|!text.is_empty()){output(text,&draft,read_only);}
                    }
                },
                Update::Outputs(page)=>{
                    if page["retention_gap_possible"]==true{eprintln!("\r\x1b[2KSome earlier output may be outside the server retained window.");}
                    for item in page["outputs"].as_array().ok_or("chat output page has no outputs")? {
                        if let Some(text)=item["text"].as_str(){output(&format!("{} · {}\n{text}",item["recipient_id"].as_str().unwrap_or("thread"),item["state"].as_str().unwrap_or("output")),&draft,read_only);}
                    }
                },
            },
            event=keyboard.next()=>match event? {
                input::Event::Detach|input::Event::End|input::Event::Interrupt=>break,
                input::Event::Resize(_,_)=>{if !read_only{draft.draw();}},
                input::Event::InputProgress(_)=>{},
                input::Event::PasteExpired=>{decoder=input::Utf8Input::default();},
                input::Event::PasteBytes(bytes)=>{if !read_only{let text=decoder.push(&bytes,false);for c in text.chars().filter(|c|!c.is_control() || matches!(c,'\n'|'\t')){if draft.text.len()+c.len_utf8()<=16*1024{draft.insert(&c.to_string());}}draft.draw();}},
                input::Event::Bytes(bytes)=>{
                    if read_only{continue;}
                    if bytes==b"\x1b[D"{draft.left();draft.draw();continue;}
                    if bytes==b"\x1b[C"{draft.right();draft.draw();continue;}
                    if bytes==b"\x1b[H"{draft.cursor=0;draft.draw();continue;}
                    if bytes==b"\x1b[F"{draft.cursor=draft.text.len();draft.draw();continue;}
                    if bytes.first()==Some(&0x1b){continue;}
                    let decoded=decoder.push(&bytes,false);
                    for c in decoded.chars(){
                        if matches!(c,'\r'|'\n') {
                            if draft.text.trim().is_empty(){continue;}
                            let text=draft.text.clone();
                            let id=match &draft.attempt{Some((old,id)) if old==&text=>id.clone(),_=>send::request_id()?};
                            draft.attempt=Some((text.clone(),id.clone()));
                            eprintln!("\r\x1b[2KSubmitting {id}; retrying unchanged input keeps this request ID.");
                            match send::submit(api,credential,&selection,&text,&id).await {
                                Ok(_)=>{draft=Draft::default();output(&format!("You → {}\n{text}",target.id()),&draft,false);eprintln!("Queued {id}");},
                                Err(error)=>{eprintln!("\r\x1b[2KInput retained ({id}): {error}");},
                            }
                        } else if matches!(c,'\u{7f}'|'\u{8}'){draft.backspace();}
                        else if c=='\u{1}'{draft.cursor=0;}
                        else if c=='\u{5}'{draft.cursor=draft.text.len();}
                        else if !c.is_control() || matches!(c,'\n'|'\t') {if draft.text.len()+c.len_utf8()<=16*1024{draft.insert(&c.to_string());}}
                    }
                    draft.draw();
                }
            }
        }
    }
    eprintln!("\r\x1b[2KDetached; remote requests were not cancelled.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chat_draft_keeps_unicode_cursor_and_pasted_newlines() {
        let mut draft = Draft::default();
        draft.insert("a界\nb");
        draft.left();
        draft.backspace();
        assert_eq!(draft.text, "a界b");
        draft.left();
        draft.insert("好");
        assert_eq!(draft.text, "a好界b");
        assert_eq!(draft.cursor, 4);
        assert_eq!(Draft::display("x\ny\tz"), "x↵y⇥z");
    }
}
