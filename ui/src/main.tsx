import { createRoot } from 'react-dom/client';
import { App } from './App';
import { startLiveSocket } from './ws';
import './styles.css';

startLiveSocket();
createRoot(document.getElementById('root')!).render(<App />);
