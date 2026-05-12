import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export default function App() {
  const [pingResult, setPingResult] = useState<string>("");

  async function handlePing() {
    try {
      const reply = await invoke<string>("ping");
      setPingResult(reply);
    } catch (err) {
      setPingResult(`error: ${err}`);
    }
  }

  return (
    <div className="app">
      <header>wos-fetch &mdash; 文献获取工具</header>
      <main className="container">
        <section className="card">
          <div className="card-header">Backend handshake</div>
          <div className="card-body">
            <p className="hint">
              脚手架阶段：点击下方按钮验证 Rust 后端是否能响应。
            </p>
            <button className="btn-primary" onClick={handlePing}>
              Ping backend
            </button>
            {pingResult && (
              <div className="status-bar status-info">{pingResult}</div>
            )}
          </div>
        </section>
      </main>
    </div>
  );
}
