# ThinkServe Switch

Windows の ThinkStation を **llama.cpp 推論サーバー** と **普段使いワークステーション** の間で、タスクトレイからすぐ切り替えるための小さなオープンソースコントローラーです。

ThinkServe Switch 自体は推論を行いません。`llama-server.exe` と Tailscale Serve を起動・停止して、GPU と外部アクセスの状態を管理します。

## 3 modes

| Mode | Tailscale remote access | llama-server | VRAM | 用途 |
|---|---|---|---|---|
| `SERVER` | ON | ON | モデルを保持 | 他端末から推論 |
| `LOCAL PRIORITY` | OFF | ON | モデルを保持 | 本体で軽作業。復帰が速い |
| `FREE GPU` | OFF | OFF | ほぼ解放 | GPUを本体作業に返す |

`FREE GPU` は現在の生成を中断して `llama-server` を終了します。VRAMを素早く返すことを優先したモードです。

## 想定構成

- Windows 11
- NVIDIA GPU
- llama.cpp の CUDA 対応 `llama-server.exe`
- GGUF モデル（例: Q6_K）
- Tailscale
- PowerShell 5.1 以上

外部公開は `llama-server` を `127.0.0.1` に固定し、その前段を Tailscale Serve にします。LAN 全体へ `0.0.0.0:8080` を直接公開しません。

> Tailscale は、所属する学校・組織・ネットワークの利用規則で許可されている環境で使ってください。

## Setup

1. llama.cpp の CUDA 対応 Windows ビルドを用意する。
2. GGUF モデルを配置する。
3. Tailscale をインストールし、ThinkStation を tailnet に参加させる。
4. `start-controller.cmd` をダブルクリックする。
5. 初回に Settings が開くので、`llama-server.exe` と `.gguf` のパスを設定する。
6. タスクトレイの ThinkServe アイコンから `SERVER` を選ぶ。

`SERVER` では内部的に概ね次の形で起動します。

```text
llama-server.exe -m <model.gguf> --host 127.0.0.1 --port 8080 -c 16384 --n-gpu-layers 99

tailscale serve --bg --https=443 127.0.0.1:8080
```

llama.cpp の OpenAI 互換 API は次のように使えます。

```text
https://<thinkstation-name>.<tailnet>.ts.net/v1/chat/completions
```

実際の `.ts.net` ホスト名は Tailscale 側で確認してください。

## Settings

- **LlamaServerPath**: `llama-server.exe`
- **ModelPath**: GGUF ファイル
- **TailscalePath**: `tailscale.exe`
- **LocalPort**: デフォルト 8080
- **TailscaleHttpsPort**: デフォルト 443
- **ContextSize**: デフォルト 16384
- **GpuLayers**: デフォルト 99（可能な限りGPUへ）
- **IdleSleepSeconds**: 0 で無効。対応する新しい llama.cpp では、指定秒数アイドル後にモデルを自動アンロード
- **ExtraArgs**: llama-server に追加したい引数
- **RestoreLastMode**: Windows 起動後に前回モードを復元

RTX 5000 Ada 32 GB + 27B Q6_K のような構成では、まず `ContextSize = 16384`, `GpuLayers = 99` から始め、VRAMに余裕があれば context を増やすのが無難です。

## Auto-start

ユーザーのログイン時にコントローラーを起動するには:

```powershell
powershell -ExecutionPolicy Bypass -File .\install-startup.ps1
```

解除:

```powershell
powershell -ExecutionPolicy Bypass -File .\uninstall-startup.ps1
```

## Logs

`logs/` に以下を保存します。

- `controller.log`
- `llama-server.stdout.log`
- `llama-server.stderr.log`

## Security notes

- llama-server は `127.0.0.1` のみに bind します。
- リモートアクセスは Tailscale Serve のみを想定しています。
- `FREE GPU` / `LOCAL PRIORITY` にすると Tailscale Serve を無効化します。
- tailnet 内のアクセス制御は Tailscale 側の ACL / grants で必要に応じて制限してください。
- `ExtraArgs` へ認証関連オプションを追加する場合、秘密情報を `config.json` に平文保存することになるため取り扱いに注意してください。

## Behavior on failures

- Tailscale が失敗しても `LOCAL PRIORITY` / `FREE GPU` への切り替えは続行します。
- `SERVER` へ切り替える際は `GET /health` が HTTP 200 になるまで待ってから Tailscale Serve を有効化します。
- コントローラーが終了しても、現在動いている llama-server はそのまま残ります。
- 次回起動時は `runtime.json` の PID を使って可能な範囲で既存プロセスを再利用します。

## License

MIT
