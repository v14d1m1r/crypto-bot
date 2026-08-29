import type { Metadata } from 'next';
import './globals.css';

export const metadata: Metadata = {
  title: 'CRUX — Crypto Bot Dashboard',
  description: 'Local monitoring dashboard for the BTC/USDT EMA paper-trading system.',
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
