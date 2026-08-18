import App from './App';
// The SDK's card stylesheet (`.smooth-chat__interaction*`), themed to this app's
// dark palette by the `--smooth-*` token overrides in styles.css.
import '@smooai/smooth-operator/react/styles.css';
import './styles.css';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

createRoot(document.getElementById('root')!).render(
    <StrictMode>
        <App />
    </StrictMode>,
);
