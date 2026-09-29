import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';
import { DiscoverModal } from './DiscoverModal';
import type { DiscoveredAccount } from '../../types';

describe('BitBrowser native takeover feedback', () => {
  it('explains native authorization and locks rescanning while takeover is pending', () => {
    const account = { source: 'bitbrowser', user_id: '123', app: 'TraeWork',
      uid_confident: true, token_present: true, in_pool: true, window_id: 'fixture' } as DiscoveredAccount;
    const html = renderToStaticMarkup(<DiscoverModal open scanning={false}
      discovered={[account]} addingUid="123" onClose={() => {}} onAdd={() => {}} onRescan={() => {}} />);
    expect(html).toContain('等待原生授权');
    expect(html).toContain('5 分钟');
    const rescan = html.match(/<button\b[^>]*>[\s\S]*?<\/button>/g)?.find((button) => button.includes('重新扫描'));
    expect(rescan).toContain('disabled=""');
  });
});
