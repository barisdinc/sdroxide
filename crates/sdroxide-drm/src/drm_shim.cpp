/* See drm_shim.h. Drives Dream's CDRMReceiver from ring buffers and flattens
   what it knows into one status struct. */

#include "drm_shim.h"
#include "sdrx_sound.h"

#include "DrmReceiver.h"
#include "GlobalDefinitions.h"
#include "Parameter.h"
#include "util/Settings.h"
#include "sourcedecoders/AudioCodec.h"

#include <cstring>
#include <string>

/* Which ring the sound objects Dream allocates should attach to. Dream news
   them up deep inside CReceiveData/CWriteData, so there is nowhere to pass it;
   a thread-local works because one decoder owns one thread. */
static thread_local CSdrxRing* tls_ring = nullptr;

CSdrxRing* sdrx_current_ring() { return tls_ring; }
void sdrx_set_current_ring(CSdrxRing* r) { tls_ring = r; }

const char* CSoundInSdrx::SDRX_DEVICE = "sdroxide";

struct sdrx_drm_ring
{
    sdrx_drm_ring(size_t in_cap, size_t out_cap) : ring(in_cap, out_cap) {}
    CSdrxRing ring;
};

struct sdrx_drm
{
    CSettings settings;
    CDRMReceiver receiver;
    CSdrxRing* ring;

    explicit sdrx_drm(CSdrxRing* r) : settings(), receiver(&settings), ring(r) {}
};

/* --- ring ---------------------------------------------------------------- */

sdrx_drm_ring* sdrx_drm_ring_new(size_t in_capacity, size_t out_capacity)
{
    return new sdrx_drm_ring(in_capacity, out_capacity);
}

void sdrx_drm_ring_free(sdrx_drm_ring* r) { delete r; }

size_t sdrx_drm_ring_push(sdrx_drm_ring* r, const int16_t* data, size_t n)
{
    return r->ring.in.push(data, n);
}

size_t sdrx_drm_ring_pop(sdrx_drm_ring* r, int16_t* data, size_t n)
{
    return r->ring.out.pop(data, n);
}

size_t sdrx_drm_ring_out_available(sdrx_drm_ring* r)
{
    return r->ring.out.available();
}

void sdrx_drm_ring_stop(sdrx_drm_ring* r) { r->ring.in.stop(); }

/* --- receiver ------------------------------------------------------------ */

sdrx_drm* sdrx_drm_new(sdrx_drm_ring* r, const sdrx_drm_config* cfg)
{
    sdrx_set_current_ring(&r->ring);

    sdrx_drm* h = nullptr;
    try
    {
        h = new sdrx_drm(&r->ring);

        CSettings& s = h->settings;
        s.Put("Receiver", "sampleratesig", int(cfg->sig_sample_rate));
        s.Put("Receiver", "samplerateaud", int(cfg->aud_sample_rate));
        /* CS_IQ_POS_ZERO: I and Q as they come off a zero-IF receiver, which
           Dream shifts to its own 6 kHz virtual IF. CS_MIX_CHAN averages the
           two channels, which is what a real-valued signal wants. */
        s.Put("Receiver", "inchansel", int(cfg->iq_input ? CS_IQ_POS_ZERO : CS_MIX_CHAN));
        s.Put("Receiver", "flipspectrum", cfg->flip_spectrum != 0);
        /* Empty means "the default device", which is the only device our
           sound shim enumerates. Anything else and CDRMReceiver::SetInputDevice
           would decide this is an RSCI network source instead. */
        s.Put("Receiver", "snddevin", std::string());
        s.Put("Receiver", "snddevout", std::string());
        /* Nothing here should touch the filesystem: no station schedules, no
           reception log, no MOT object cache. */
        s.Put("Receiver", "datafilesdirectory", std::string("."));
        s.Put("command", "mode", std::string("receive"));

        h->receiver.LoadSettings();
        h->receiver.SetReceiverMode(RM_DRM);
        h->receiver.InitReceiverMode();
        h->receiver.SetInStartMode();
    }
    catch (CGenErr&)
    {
        delete h;
        return nullptr;
    }
    catch (std::string&)
    {
        delete h;
        return nullptr;
    }
    return h;
}

void sdrx_drm_free(sdrx_drm* h)
{
    if (h == nullptr)
        return;
    sdrx_set_current_ring(h->ring);
    try
    {
        h->receiver.CloseSoundInterfaces();
    }
    catch (...)
    {
    }
    delete h;
    sdrx_set_current_ring(nullptr);
}

int32_t sdrx_drm_process(sdrx_drm* h)
{
    sdrx_set_current_ring(h->ring);
    try
    {
        h->receiver.updatePosition();
        h->receiver.process();
    }
    catch (CGenErr&)
    {
        return -1;
    }
    catch (std::string&)
    {
        return -1;
    }
    return 0;
}

void sdrx_drm_restart(sdrx_drm* h)
{
    sdrx_set_current_ring(h->ring);
    try
    {
        h->receiver.InitReceiverMode();
        h->receiver.SetInStartMode();
    }
    catch (...)
    {
    }
}

void sdrx_drm_select_service(sdrx_drm* h, int32_t service)
{
    if (service < 0 || service >= MAX_NUM_SERVICES)
        return;
    CParameter& p = *h->receiver.GetParameters();
    p.Lock();
    p.SetCurSelAudioService(int(service));
    p.Unlock();
}

/* 4-QAM, 16-QAM or 64-QAM, from the coding scheme the transmission signalled. */
static int32_t qam_order(ECodScheme scheme)
{
    switch (scheme)
    {
    case CS_1_SM: return 4;
    case CS_2_SM: return 16;
    default:      return 64;
    }
}

int32_t sdrx_drm_constellation(sdrx_drm* h, int32_t channel, float* out,
                               int32_t max_points, int32_t* qam)
{
    if (out == nullptr || max_points <= 0)
        return 0;

    CVector<_COMPLEX> cells;
    CParameter& p = *h->receiver.GetParameters();

    p.Lock();
    ECodScheme sdc = p.eSDCCodingScheme;
    ECodScheme msc = p.eMSCCodingScheme;
    p.Unlock();

    switch (channel)
    {
    case SDRX_DRM_CHANNEL_FAC:
        /* The FAC is 4-QAM in every transmission there is; it has to be
           readable before anything says what the rest of the multiplex uses. */
        h->receiver.GetFACMLC()->GetVectorSpace(cells);
        if (qam) *qam = 4;
        break;
    case SDRX_DRM_CHANNEL_SDC:
        h->receiver.GetSDCMLC()->GetVectorSpace(cells);
        if (qam) *qam = qam_order(sdc);
        break;
    default:
        h->receiver.GetMSCMLC()->GetVectorSpace(cells);
        if (qam) *qam = qam_order(msc);
        break;
    }

    const int32_t have = int32_t(cells.Size());
    if (have <= 0)
        return 0;

    const int32_t want = have < max_points ? have : max_points;
    for (int32_t i = 0; i < want; i++)
    {
        /* Even stride over the whole frame — see the header. */
        const int32_t src = int32_t((int64_t(i) * have) / want);
        out[2 * i] = float(cells[src].real());
        out[2 * i + 1] = float(cells[src].imag());
    }
    return want;
}

static void copy_str(char* dst, size_t cap, const std::string& src)
{
    size_t n = src.size() < cap - 1 ? src.size() : cap - 1;
    std::memcpy(dst, src.data(), n);
    dst[n] = '\0';
}

void sdrx_drm_get_status(sdrx_drm* h, sdrx_drm_status* out)
{
    std::memset(out, 0, sizeof(*out));
    out->robustness_mode = -1;
    out->spectrum_occupancy = -1;
    out->doppler_hz = -1.0;

    CParameter& p = *h->receiver.GetParameters();
    p.Lock();

    ETypeRxStatus in_st = p.ReceiveStatus.InterfaceI.GetStatus();
    ETypeRxStatus out_st = p.ReceiveStatus.InterfaceO.GetStatus();
    /* Dream shows one IO light: the input's problem if it has one, else the
       output's. */
    out->io_status = int32_t(out_st == NOT_PRESENT ||
                             (in_st != NOT_PRESENT && in_st != RX_OK) ? in_st : out_st);
    out->time_sync_status = int32_t(p.ReceiveStatus.TSync.GetStatus());
    out->frame_sync_status = int32_t(p.ReceiveStatus.FSync.GetStatus());
    out->fac_status = int32_t(p.ReceiveStatus.FAC.GetStatus());
    out->sdc_status = int32_t(p.ReceiveStatus.SDC.GetStatus());
    out->audio_status = int32_t(p.ReceiveStatus.SLAudio.GetStatus());

    out->if_level_db = double(p.GetIFSignalLevel());
    out->has_signal = h->receiver.GetAcquiState() == AS_WITH_SIGNAL ? 1 : 0;
    out->audio_sample_rate_out = int32_t(p.GetAudSampleRate());

    if (out->has_signal)
    {
        out->snr_db = double(p.GetSNR());
        out->wmer_db = double(p.rWMERMSC);
        out->mer_db = double(p.rMER);
        out->dc_frequency_hz = double(p.GetDCFrequency());
        out->sample_offset_hz = double(p.rResampleOffset);
        if (p.rSigmaEstimate >= 0.0)
        {
            out->doppler_hz = double(p.rSigmaEstimate);
            out->delay_ms = double(p.rMinDelay);
        }

        ERobMode rm = p.GetWaveMode();
        if (rm != RM_NO_MODE_DETECTED)
            out->robustness_mode = int32_t(rm);
        out->spectrum_occupancy = int32_t(p.GetSpectrumOccup());
        out->interleaver_long = p.eSymbolInterlMode == CParameter::SI_LONG ? 1 : 0;
        out->sdc_scheme = int32_t(p.eSDCCodingScheme);
        out->msc_scheme = int32_t(p.eMSCCodingScheme);
        out->prot_level_a = int32_t(p.MSCPrLe.iPartA);
        out->prot_level_b = int32_t(p.MSCPrLe.iPartB);
        out->num_audio_services = int32_t(p.iNumAudioService);
        out->num_data_services = int32_t(p.iNumDataService);
        out->year = int32_t(p.iYear);
        out->month = int32_t(p.iMonth);
        out->day = int32_t(p.iDay);
        out->utc_hour = int32_t(p.iUTCHour);
        out->utc_minute = int32_t(p.iUTCMin);
    }

    const int cur = p.GetCurSelAudioService();
    out->cur_service = int32_t(cur);
    if (cur >= 0 && cur < MAX_NUM_SERVICES)
    {
        const CService& service = p.Service[cur];
        if (service.IsActive())
        {
            copy_str(out->label, sizeof(out->label), service.strLabel);
            copy_str(out->country_code, sizeof(out->country_code), service.strCountryCode);
            copy_str(out->language_code, sizeof(out->language_code), service.strLanguageCode);
            copy_str(out->text_message, sizeof(out->text_message),
                     service.AudioParam.strTextMessage);
            out->service_id = int32_t(service.iServiceID);
            out->bitrate_kbps =
                double(p.GetBitRateKbps(cur, service.eAudDataFlag != CService::SF_AUDIO));
            out->audio_codec = int32_t(service.AudioParam.eAudioCoding);
            out->audio_mode = int32_t(service.AudioParam.eAudioMode);
            out->audio_sample_rate = int32_t(service.AudioParam.eAudioSamplRate);
            out->is_stereo = service.AudioParam.eAudioMode == CAudioParam::AM_STEREO ? 1 : 0;
        }
    }

    p.Unlock();
}

const char* sdrx_drm_codec_version(void)
{
    static std::string version;
    if (version.empty())
    {
        CAudioCodec* codec = CAudioCodec::GetDecoder(CAudioParam::AC_AAC, false);
        version = codec != nullptr ? codec->DecGetVersion() : std::string();
    }
    return version.c_str();
}
