/**
 * Copy text to the desktop clipboard without requiring a browser permission
 * prompt when the app is running in the Tauri webview.
 *
 * The fallback is intentionally limited to the current document and is only
 * used when the async Clipboard API is unavailable or rejected.
 */
export async function copyTextToClipboard(text: string): Promise<void> {
  if (!text) throw new Error('复制内容为空');
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // Tauri/WebView permissions can reject the async API; use the local
      // textarea fallback below instead of silently reporting success.
    }
  }

  const textarea = document.createElement('textarea');
  textarea.value = text;
  textarea.setAttribute('readonly', '');
  textarea.style.position = 'fixed';
  textarea.style.opacity = '0';
  textarea.style.pointerEvents = 'none';
  document.body.appendChild(textarea);
  textarea.select();
  try {
    if (!document.execCommand('copy')) throw new Error('系统剪贴板拒绝复制');
  } finally {
    document.body.removeChild(textarea);
  }
}
