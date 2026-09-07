'use client';

import { useCallback, useEffect, useRef, useState } from 'react';

type BotStatus = { cash:number; equity:number; position_quantity:number|null; entry_price:number|null; last_price:number; updated_at:number };
type Config = { symbol:string; interval:string; fast_ema:number; slow_ema:number; starting_cash:number; mode:string; position_fraction?:number; max_order_quote?:number };
type StatusResponse = { bot:BotStatus|null; config:Config; budget?:{ initial_equity:number; started_at:number }|null };
type Trade = { id:number; timestamp:number; side:string; price:number; quantity:number; fee:number; realized_pnl:number; reason:string };
type Equity = { timestamp:number; equity:number };
type Candle = { close_time:number; close:number };

const API = process.env.NEXT_PUBLIC_API_URL ?? 'http://127.0.0.1:3001';
const fallbackConfig: Config = { symbol:'BTCUSDT', interval:'1m', fast_ema:20, slow_ema:50, starting_cash:10000, mode:'paper' };
const money = (value = 0) => new Intl.NumberFormat('en-US', { style:'currency', currency:'USD', maximumFractionDigits:2 }).format(value);

function LineChart({ values, color, label }:{ values:number[]; color:string; label:string }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || values.length < 2) return;
    const draw = () => {
      const rect = canvas.getBoundingClientRect();
      const ratio = window.devicePixelRatio || 1;
      canvas.width = rect.width * ratio; canvas.height = rect.height * ratio;
      const ctx = canvas.getContext('2d'); if (!ctx) return;
      ctx.scale(ratio, ratio);
      const min = Math.min(...values), max = Math.max(...values), range = max - min || 1;
      ctx.clearRect(0, 0, rect.width, rect.height);
      ctx.strokeStyle = 'rgba(255,255,255,.06)'; ctx.lineWidth = 1;
      for (let i=1;i<4;i++) { const y=(rect.height/4)*i; ctx.beginPath();ctx.moveTo(0,y);ctx.lineTo(rect.width,y);ctx.stroke(); }
      ctx.strokeStyle=color;ctx.lineWidth=2.5;ctx.lineJoin='round';ctx.beginPath();
      values.forEach((value,index) => { const x=(index/(values.length-1))*rect.width;const y=rect.height-10-((value-min)/range)*(rect.height-20);if(index===0){ctx.moveTo(x,y)}else{ctx.lineTo(x,y)} });
      ctx.stroke();
    };
    draw(); const observer=new ResizeObserver(draw);observer.observe(canvas);return()=>observer.disconnect();
  }, [values,color]);
  return <canvas ref={canvasRef} className="chart" role="img" aria-label={label}/>;
}

export default function Home() {
  const [status,setStatus]=useState<StatusResponse>({bot:null,config:fallbackConfig});
  const [trades,setTrades]=useState<Trade[]>([]),[equity,setEquity]=useState<Equity[]>([]),[candles,setCandles]=useState<Candle[]>([]);
  const [online,setOnline]=useState(false);
  const refresh=useCallback(async()=>{try{
    const responses=await Promise.all(['status','trades','equity','candles'].map(path=>fetch(`${API}/api/${path}`)));
    if(!responses.every(response=>response.ok))throw new Error('API unavailable');
    const [nextStatus,nextTrades,nextEquity,nextCandles]=await Promise.all(responses.map(response=>response.json()));
    setStatus(nextStatus);setTrades(nextTrades);setEquity(nextEquity);setCandles(nextCandles);setOnline(true);
  }catch{setOnline(false)}},[]);
  useEffect(()=>{const initial=window.setTimeout(refresh,0);const timer=window.setInterval(refresh,5000);return()=>{window.clearTimeout(initial);window.clearInterval(timer)}},[refresh]);
  const baseline=status.budget?.initial_equity??status.config.starting_cash;
  const bot=status.bot,pnl=bot?bot.equity-baseline:0,pnlPercent=bot&&baseline>0?(pnl/baseline)*100:0;
  const lastUpdate=bot?new Date(bot.updated_at).toLocaleTimeString([],{hour:'2-digit',minute:'2-digit'}):'Waiting';

  return <main className="shell">
    <header className="topbar"><div className="brand"><span className="brand-mark">C</span><div><strong>CRUX</strong><small>MARKET SYSTEMS</small></div></div><div className="market"><span>{status.config.symbol.replace('USDT','')}</span><i>/</i><span>USDT</span><em>{status.config.interval}</em></div><div className={`connection ${online?'online':''}`}><span/>{online?'SYSTEM LIVE':'API OFFLINE'}</div></header>
    <section className="hero"><div><p className="eyebrow">{status.config.mode==='testnet'?'BINANCE TESTNET PORTFOLIO':'AUTONOMOUS PAPER PORTFOLIO'}</p><h1>Signal, execution,<br/><span>without the noise.</span></h1></div><div className="strategy-badge"><small>ACTIVE MODEL</small><strong>EMA {status.config.fast_ema} <i>×</i> EMA {status.config.slow_ema}</strong><p>Long-only crossover · risk managed</p></div></section>
    {!online&&<div className="notice"><strong>Dashboard API is offline.</strong> Start <code>cargo run -- bot</code>; this screen reconnects automatically.</div>}
    {status.budget&&<div className="notice">Initial allocation: <strong>{money(baseline)}</strong>. Equity and P&amp;L track this bot&apos;s budget since {new Date(status.budget.started_at).toLocaleString()}; other Testnet funds are excluded. Per-order cap: {money(status.config.max_order_quote??25)}.</div>}
    <section className="metrics">
      <article><label>{status.budget?'BOT BUDGET EQUITY':'NET EQUITY'}</label><strong>{money(bot?.equity??baseline)}</strong><span className={pnl>=0?'positive':'negative'}>{pnl>=0?'↑':'↓'} {pnlPercent.toFixed(2)}%</span></article>
      <article><label>{status.budget?'BOT CASH':'AVAILABLE CASH'}</label><strong>{money(bot?.cash??baseline)}</strong><span>{((status.config.position_fraction??0.25)*100).toFixed(0)}% of available cash per buy</span></article>
      <article><label>LAST PRICE</label><strong>{bot?money(bot.last_price):'—'}</strong><span>Updated {lastUpdate}</span></article>
      <article><label>POSITION</label><strong>{bot?.position_quantity?bot.position_quantity.toFixed(6):'FLAT'}</strong><span>{bot?.entry_price?`Entry ${money(bot.entry_price)}`:'Waiting for signal'}</span></article>
    </section>
    <section className="grid">
      <article className="panel performance"><div className="panel-head"><div><small>PORTFOLIO</small><h2>Equity curve</h2></div><span>LAST {equity.length} CANDLES</span></div>{equity.length>1?<LineChart values={equity.map(point=>point.equity)} color="#b8ff57" label="Portfolio equity over time"/>:<div className="empty-chart">Equity history appears after closed candles</div>}<div className="chart-footer"><span>START {money(baseline)}</span><strong className={pnl>=0?'positive':'negative'}>{pnl>=0?'+':''}{money(pnl)}</strong></div></article>
      <article className="panel price-panel"><div className="panel-head"><div><small>MARKET</small><h2>{status.config.symbol} close</h2></div><span>BINANCE · {status.config.interval}</span></div>{candles.length>1?<LineChart values={candles.map(candle=>candle.close)} color="#ff8b5c" label={`${status.config.symbol} closing prices`}/>:<div className="empty-chart">Price history appears after closed candles</div>}<div className="risk-row"><div><small>STOP LOSS</small><b>−2.00%</b></div><div><small>TAKE PROFIT</small><b>+4.00%</b></div><div><small>FEE MODEL</small><b>0.10%</b></div></div></article>
    </section>
    <section className="panel ledger"><div className="panel-head"><div><small>EXECUTION LEDGER</small><h2>{status.config.mode==='testnet'?'Recent Testnet trades':'Recent paper trades'}</h2></div><span>{trades.length} RECORDED</span></div><div className="table-wrap"><table><thead><tr><th>TIME</th><th>SIDE</th><th>PRICE</th><th>QUANTITY</th><th>REALIZED P&amp;L</th><th>EXIT LOGIC</th></tr></thead><tbody>{trades.length?trades.map(trade=><tr key={trade.id}><td>{new Date(trade.timestamp).toLocaleString()}</td><td><b className={`side ${trade.side.toLowerCase()}`}>{trade.side}</b></td><td>{money(trade.price)}</td><td>{trade.quantity.toFixed(6)}</td><td className={trade.realized_pnl>=0?'positive':'negative'}>{money(trade.realized_pnl)}</td><td>{trade.reason}</td></tr>):<tr><td colSpan={6} className="empty-row">No trades yet. The EMA crossover is monitoring closed candles.</td></tr>}</tbody></table></div></section>
    <footer><span>{status.config.mode.toUpperCase()} ENVIRONMENT</span><p>Market data by Binance · State persisted in SQLite</p><button onClick={refresh}>REFRESH DATA</button></footer>
  </main>;
}
