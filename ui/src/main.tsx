import { createRoot } from 'react-dom/client';
import { App } from './App';
import { initTheme } from './theme';
import { startLiveSocket } from './ws';
import './styles.css';

initTheme();
startLiveSocket();
createRoot(document.getElementById('root')!).render(<App />);
