import { createRoot } from 'react-dom/client';
import { App } from './App';
import { TokenPrompt } from './components/TokenPrompt';
import { initTheme } from './theme';
import { startLiveSocket } from './ws';
import './fonts.css';
import './styles.css';

initTheme();
startLiveSocket();
createRoot(document.getElementById('root')!).render(
  <>
    <App />
    <TokenPrompt />
  </>,
);
