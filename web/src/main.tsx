import {StrictMode} from 'react';
import {createRoot} from 'react-dom/client';
import {QueryClientProvider} from '@tanstack/react-query';
import {TooltipProvider} from '@/components/ui/tooltip';
import {queryClient} from '@/lib/api';
import {NarratorProvider} from '@/state';
import App from './App';
import './index.css';

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <TooltipProvider delayDuration={400}>
        <NarratorProvider>
          <App />
        </NarratorProvider>
      </TooltipProvider>
    </QueryClientProvider>
  </StrictMode>,
);
