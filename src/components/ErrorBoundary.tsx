import { Component, type ErrorInfo, type ReactNode } from "react";

/**
 * A render error anywhere below would otherwise leave an empty white window. Shows what went
 * wrong and lets the user open the screen again; files are not affected (all work is done by the
 * backend, which keeps running).
 */
export default class ErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null; info: string }> {
  state = { error: null as Error | null, info: "" };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    this.setState({ info: info.componentStack ?? "" });
    console.error(error, info.componentStack);
  }

  render() {
    const { error, info } = this.state;
    if (!error) return this.props.children;
    return (
      <div className="crash">
        <h2>화면을 그리다 오류가 났습니다</h2>
        <p>사진 파일에는 영향이 없습니다. 진행 중이던 작업은 뒤에서 계속됩니다.</p>
        <div className="btn-row">
          <button className="btn primary" onClick={() => this.setState({ error: null, info: "" })}>다시 열기</button>
          <button className="btn" onClick={() => window.location.reload()}>앱 화면 새로 고침</button>
        </div>
        <details>
          <summary>오류 내용 (문의할 때 보내 주세요)</summary>
          <pre>{String(error?.stack ?? error)}{info}</pre>
        </details>
      </div>
    );
  }
}
